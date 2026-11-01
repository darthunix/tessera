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
//! fixed-seed generator and show it, as long as the predictor does not learn
//! the one input of the case. A case of several inputs ([`Fixture::ring`])
//! takes, call after call, the same words in other orders, so that it does
//! the same work but repeats neither where its rows lie nor how many the n-th
//! word holds.

use super::reference::Mask;
use anyhow::Result;
use std::collections::BTreeMap;
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
        Self::with_seed(nrows as u64)
    }
    fn with_seed(seed: u64) -> Self {
        Self(0x9E37_79B9_7F4A_7C15 ^ seed.wrapping_mul(0xD1B5_4A32_D192_ED03) | 1)
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

/// `values` in an order drawn from a fixed-seed generator.
pub fn shuffled<T>(mut values: Vec<T>) -> Vec<T> {
    let mut random = Random::seeded(values.len());
    shuffle(&mut values, &mut random);
    values
}

/// `items` in an order drawn from `random` (Fisher and Yates).
fn shuffle<T>(items: &mut [T], random: &mut Random) {
    for at in (1..items.len()).rev() {
        items.swap(at, (random.next() % (at as u64 + 1)) as usize);
    }
}

/// The misses a call of a loop over the selected rows of each word: a
/// predictor that knows how the loop's trip counts spread over the words,
/// but not the input, foretells the commonest count and misses the exit of
/// every word that holds another. Words with no selected row, or with all
/// of them, take no such loop.
pub fn exit_misses(selected: &Bitmap) -> u64 {
    let mut counts = BTreeMap::new();
    let mut words = 0;
    for (index, &word) in selected.words.iter().enumerate() {
        let len = (selected.nrows - index * 64).min(64) as u32;
        let count = word.count_ones();
        if count != 0 && count != len {
            *counts.entry(count).or_insert(0) += 1;
            words += 1;
        }
    }
    words - counts.values().max().copied().unwrap_or(0)
}

/// The misses a call of the search by halving of `keys` over `values`: at
/// each step, the values that take its rarer way, as a predictor that
/// knows each step's odds, but not the values, would miss them. The steps
/// are those of `tessera_kernels::set::KeySet::contains` beyond the keys it
/// compares all.
pub fn halving_misses<T: Ord + Copy>(keys: &[T], values: &[T]) -> u64 {
    let mut steps: Vec<[u64; 2]> = Vec::new();
    for &value in values {
        let (mut base, mut size, mut step) = (0, keys.len(), 0);
        while size > 1 {
            let half = size / 2;
            let middle = base + half;
            let right = keys[middle] <= value;
            if steps.len() == step {
                steps.push([0; 2]);
            }
            steps[step][usize::from(right)] += 1;
            base = if right { middle } else { base };
            size -= half;
            step += 1;
        }
    }
    steps.iter().map(|ways| ways[0].min(ways[1])).sum()
}

#[derive(Clone)]
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

    /// The same rows, masks and name with each value mapped, such as the
    /// values of a smallint kernel brought into its range.
    pub fn map_values(&self, map: impl Fn(T) -> T) -> Self {
        let values: Vec<T> = self.values.iter().map(|&value| map(value)).collect();
        let selected = self.selected.words();
        let expected = values
            .iter()
            .enumerate()
            .filter(|&(row, _)| selected[row / 64] & (1 << (row % 64)) != 0 && !self.nulls[row])
            .map(|(_, &value)| value.wide())
            .fold(0, i64::wrapping_add);
        Self {
            name: self.name.clone(),
            datums: values.iter().map(|&value| value.datum()).collect(),
            values,
            nulls: self.nulls.clone(),
            selected: self.selected.clone(),
            prepared: self.prepared.clone(),
            non_nulls: self.non_nulls.clone(),
            expected,
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
                        "quarter" => row % 4 == 0,
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

    /// The inputs of a case that the branch predictor cannot learn: `self`,
    /// then `count - 1` inputs made by [`Self::permuted`], each with a seed
    /// of its own.
    pub fn ring(self, count: usize) -> Vec<Self> {
        let mut ring = Vec::with_capacity(count);
        for seed in 1..count as u64 {
            ring.push(self.permuted(seed));
        }
        ring.insert(0, self);
        ring
    }

    /// The same words of 64 rows in an order drawn from `seed`, then the
    /// rows of each word in an order drawn from it too, every buffer and
    /// mask moved together: the same words, so the same work, but neither
    /// where the rows lie nor how many the n-th word holds is the same. A
    /// last word of fewer rows stays last, its rows permuted.
    pub fn permuted(&self, seed: u64) -> Self {
        let nrows = self.values.len();
        let full = nrows / 64;
        let mut random = Random::with_seed(seed);
        let mut words: Vec<_> = (0..full).collect();
        shuffle(&mut words, &mut random);
        let mut order = Vec::with_capacity(nrows);
        for word in words.into_iter().chain((full * 64 < nrows).then_some(full)) {
            let mut rows: Vec<_> = (word * 64..(word * 64 + 64).min(nrows)).collect();
            shuffle(&mut rows, &mut random);
            order.extend(rows);
        }
        let moved = |bitmap: &Bitmap| {
            let flags: Vec<_> = order
                .iter()
                .map(|&row| bitmap.words[row / 64] & (1 << (row % 64)) != 0)
                .collect();
            Bitmap::new(&flags)
        };
        Self {
            name: self.name.clone(),
            values: order.iter().map(|&row| self.values[row]).collect(),
            datums: order.iter().map(|&row| self.datums[row]).collect(),
            nulls: order.iter().map(|&row| self.nulls[row]).collect(),
            selected: moved(&self.selected),
            prepared: self.prepared.as_ref().map(moved),
            non_nulls: self.non_nulls.as_ref().map(moved),
            expected: self.expected,
        }
    }

    /// One input of the rows of `inputs`, one after another, so that a call
    /// over it does the work of a call over each in turn. Every input but
    /// the last holds whole words, so that its words stay words; the name is
    /// the first input's.
    pub fn concat(inputs: &[Self]) -> Self {
        let rows = |input: &Self| input.values.len();
        assert!(
            inputs
                .split_last()
                .is_some_and(|(_, init)| init.iter().all(|input| rows(input) % 64 == 0)),
            "inputs to join in words"
        );
        let flags = |bitmap: &Bitmap| -> Vec<bool> {
            (0..bitmap.nrows)
                .map(|row| bitmap.words[row / 64] & (1 << (row % 64)) != 0)
                .collect()
        };
        let joined = |bitmap: fn(&Self) -> Option<&Bitmap>| {
            bitmap(&inputs[0]).is_some().then(|| {
                Bitmap::new(
                    &inputs
                        .iter()
                        .flat_map(|input| flags(bitmap(input).unwrap()))
                        .collect::<Vec<_>>(),
                )
            })
        };
        Self {
            name: inputs[0].name.clone(),
            values: inputs
                .iter()
                .flat_map(|input| input.values.clone())
                .collect(),
            datums: inputs
                .iter()
                .flat_map(|input| input.datums.clone())
                .collect(),
            nulls: inputs
                .iter()
                .flat_map(|input| input.nulls.clone())
                .collect(),
            selected: joined(|input| Some(&input.selected)).unwrap(),
            prepared: joined(|input| input.prepared.as_ref()),
            non_nulls: joined(|input| input.non_nulls.as_ref()),
            expected: inputs
                .iter()
                .map(|input| input.expected)
                .fold(0, i64::wrapping_add),
        }
    }
}
