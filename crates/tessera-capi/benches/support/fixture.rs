//! Deterministic inputs shared by reading/filtering and their scalar references.
//!
//! Build dense and Datum buffers, selection/NULL/readiness masks, and an expected
//! sum before timing. The full matrix covers boundary sizes and mask layouts.
//! All fixture buffers are initialized, even NULL and unprepared positions:
//! uninitialized-buffer safety belongs to the library's correctness/Miri tests.
//! Columns and references borrow the same allocations, not copies of the values.
//!
//! Regular patterns (`half`, `sparse`, `nulls-mixed`) are what a branch
//! predictor learns perfectly, so they hide the cost of data-dependent
//! branches; the `random` selection and `nulls-random` flags come from a
//! fixed-seed generator and show it.

use super::reference::Mask;
use anyhow::Result;
use std::mem::MaybeUninit;
use tessera_capi::{DatumIntColumn, DenseIntColumn, FromDatum};
use tessera_core::RowMaskView;

/// A value a fixture holds: the int4 or int8 of a Datum, in both
/// representations.
pub trait FixtureValue: FromDatum {
    /// The Datum PostgreSQL makes of the value (sign-extended, as
    /// Int32GetDatum and Int64GetDatum do).
    fn datum(self) -> u64;
    /// The value widened for sums.
    fn wide(self) -> i64;
}

impl FixtureValue for i32 {
    fn datum(self) -> u64 {
        self as i64 as u64
    }
    fn wide(self) -> i64 {
        i64::from(self)
    }
}

impl FixtureValue for i64 {
    fn datum(self) -> u64 {
        self as u64
    }
    fn wide(self) -> i64 {
        self
    }
}

/// xorshift64*: deterministic, and its high bits are well mixed.
struct Random(u64);

impl Random {
    fn seeded(nrows: usize) -> Self {
        Self(0x9E37_79B9_7F4A_7C15 ^ (nrows as u64).wrapping_mul(0xD1B5_4A32_D192_ED03) | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// True with probability `percent`/100.
    fn chance(&mut self, percent: u64) -> bool {
        (self.next() >> 32) % 100 < percent
    }
}

pub struct Bitmap {
    nrows: usize,
    words: Vec<u64>,
}

impl Bitmap {
    fn new(flags: &[bool]) -> Self {
        let mut words = vec![0; flags.len().div_ceil(64)];
        for (row, &flag) in flags.iter().enumerate() {
            if flag {
                words[row / 64] |= 1 << (row % 64);
            }
        }
        Self {
            nrows: flags.len(),
            words,
        }
    }
    pub fn view(&self) -> RowMaskView<'_> {
        RowMaskView::try_new(self.nrows, &self.words).unwrap()
    }
    pub fn words(&self) -> &[u64] {
        &self.words
    }
    pub fn reference(&self) -> Mask<'_> {
        Mask {
            nrows: self.nrows,
            words: &self.words,
        }
    }
}

/// The inputs of one case; int4 unless a width is named.
pub struct Fixture<T = i32> {
    pub name: String,
    pub values: Vec<T>,
    pub datums: Vec<u64>,
    pub nulls: Vec<bool>,
    pub selected: Bitmap,
    pub prepared: Option<Bitmap>,
    pub non_nulls: Option<Bitmap>,
    pub expected: i64,
}

/// Borrow initialized values in the representation accepted by column constructors.
pub fn as_uninit<T>(values: &[T]) -> &[MaybeUninit<T>] {
    // SAFETY: MaybeUninit<T> has T's size and alignment, and accepts all valid T
    // values. The shared slice keeps the original length and lifetime and cannot
    // be used to make any value uninitialized or otherwise mutate the buffer.
    unsafe { std::slice::from_raw_parts(values.as_ptr().cast(), values.len()) }
}

impl<T: FixtureValue> Fixture<T> {
    pub fn dense_column(&self) -> Result<DenseIntColumn<'_, T>> {
        // SAFETY: fixture buffers are initialized, immutable throughout the borrow,
        // and outlive the returned column, including NULL and unprepared positions.
        unsafe {
            DenseIntColumn::try_new(
                as_uninit(&self.values),
                self.non_nulls.as_ref().map(Bitmap::view),
                self.prepared.as_ref().map(Bitmap::view),
            )
        }
    }

    pub fn datum_column(&self) -> Result<DatumIntColumn<'_, T>> {
        // SAFETY: the same fixture initialization and lifetime guarantees hold
        // for Datum values and valid bool flags.
        unsafe {
            DatumIntColumn::try_new(
                as_uninit(&self.datums),
                as_uninit(&self.nulls),
                self.prepared.as_ref().map(Bitmap::view),
            )
        }
    }

    pub fn from_values(values: Vec<T>, pattern: &str, nulls: &str, partial: bool) -> Self {
        let nrows = values.len();
        let mut random = Random::seeded(nrows);
        let ready: Vec<_> = (0..nrows).map(|row| !partial || row % 3 != 2).collect();
        let selected: Vec<_> = (0..nrows)
            .map(|row| {
                ready[row]
                    && match pattern {
                        "all" => true,
                        "half" => row % 2 == 0,
                        "sparse" => row % 64 == 0,
                        "eighth" => row % 8 == 0,
                        "one-per128" => row % 128 == 0,
                        "empty" => false,
                        "random" => random.chance(50),
                        _ => unreachable!(),
                    }
            })
            .collect();
        let flags: Vec<_> = (0..nrows)
            .map(|row| match nulls {
                "none" => false,
                "mixed" => row % 5 == 1,
                "all" => true,
                "random" => random.chance(30),
                _ => unreachable!(),
            })
            .collect();
        let datums: Vec<_> = values.iter().map(|&value| value.datum()).collect();
        let expected = values
            .iter()
            .enumerate()
            .filter(|&(row, _)| selected[row] && !flags[row])
            .map(|(_, &value)| value.wide())
            // Wrapping: an int8 fixture may hold both extremes.
            .fold(0, i64::wrapping_add);
        Self {
            name: format!(
                "words/{nrows}/{pattern}/nulls-{nulls}/{}",
                if partial { "partial" } else { "ready" }
            ),
            non_nulls: (nulls != "none")
                .then(|| Bitmap::new(&flags.iter().map(|&flag| !flag).collect::<Vec<_>>())),
            prepared: partial.then(|| Bitmap::new(&ready)),
            selected: Bitmap::new(&selected),
            nulls: flags,
            values,
            datums,
            expected,
        }
    }
}
