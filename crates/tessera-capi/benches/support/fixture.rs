//! Deterministic inputs shared by reading/filtering and their scalar references.
//!
//! Build dense and Datum buffers, selection/NULL/readiness masks, and an expected
//! sum before timing. The full matrix covers boundary sizes and mask layouts;
//! quick cases are a representative subset, not a replacement for that matrix.
//! All fixture buffers are initialized, even NULL and unprepared positions:
//! uninitialized-buffer safety belongs to the library's correctness/Miri tests.
//! Columns and references borrow the same allocations, not copies of the values.

use super::reference::{Bits, Mask};
use anyhow::Result;
use std::mem::MaybeUninit;
use tessera_capi::{DatumInt32Column, DenseInt32Column};
use tessera_core::RowMaskView;

pub struct Bitmap {
    nrows: usize,
    words: Vec<u64>,
    bytes: Vec<u8>,
    offset: Option<usize>,
}

impl Bitmap {
    fn new(flags: &[bool], offset: Option<usize>) -> Self {
        let mut words = vec![0; flags.len().div_ceil(64)];
        let start = offset.unwrap_or(0);
        let mut bytes = vec![u8::MAX; (start + flags.len()).div_ceil(8)];
        for (row, &flag) in flags.iter().enumerate() {
            if flag {
                words[row / 64] |= 1 << (row % 64);
            } else {
                bytes[(start + row) / 8] &= !(1 << ((start + row) % 8));
            }
        }
        Self {
            nrows: flags.len(),
            words,
            bytes,
            offset,
        }
    }
    pub fn view(&self) -> RowMaskView<'_> {
        match self.offset {
            Some(offset) => RowMaskView::try_from_bytes(self.nrows, &self.bytes, offset).unwrap(),
            None => RowMaskView::try_new(self.nrows, &self.words).unwrap(),
        }
    }
    pub fn words(&self) -> &[u64] {
        &self.words
    }
    pub fn reference(&self) -> Mask<'_> {
        Mask {
            nrows: self.nrows,
            bits: match self.offset {
                Some(offset) => Bits::Bytes(&self.bytes, offset),
                None => Bits::Words(&self.words),
            },
        }
    }
}

pub struct Fixture {
    pub name: String,
    pub quick: bool,
    pub values: Vec<i32>,
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

impl Fixture {
    pub fn dense_column(&self) -> Result<DenseInt32Column<'_>> {
        // SAFETY: fixture buffers are initialized, immutable throughout the borrow,
        // and outlive the returned column, including NULL and unprepared positions.
        unsafe {
            DenseInt32Column::try_new(
                as_uninit(&self.values),
                self.non_nulls.as_ref().map(Bitmap::view),
                self.prepared.as_ref().map(Bitmap::view),
            )
        }
    }

    pub fn datum_column(&self) -> Result<DatumInt32Column<'_>> {
        // SAFETY: the same fixture initialization and lifetime guarantees hold
        // for Datum values and valid bool flags.
        unsafe {
            DatumInt32Column::try_new(
                as_uninit(&self.datums),
                as_uninit(&self.nulls),
                self.prepared.as_ref().map(Bitmap::view),
            )
        }
    }

    pub fn from_values(
        values: Vec<i32>,
        pattern: &str,
        nulls: &str,
        offset: Option<usize>,
        partial: bool,
    ) -> Self {
        let nrows = values.len();
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
                        _ => unreachable!(),
                    }
            })
            .collect();
        let flags: Vec<_> = (0..nrows)
            .map(|row| match nulls {
                "none" => false,
                "mixed" => row % 5 == 1,
                "all" => true,
                _ => unreachable!(),
            })
            .collect();
        let datums: Vec<_> = values.iter().map(|&value| value as u64).collect();
        let expected = values
            .iter()
            .enumerate()
            .filter(|&(row, _)| selected[row] && !flags[row])
            .map(|(_, &value)| i64::from(value))
            .sum();
        Self {
            name: format!(
                "{}/{nrows}/{pattern}/nulls-{nulls}/{}",
                offset.map_or_else(|| "words".to_owned(), |offset| format!("bytes-{offset}")),
                if partial { "partial" } else { "ready" }
            ),
            quick: false, // The owning benchmark chooses its diagnostic subset.
            non_nulls: (nulls != "none")
                .then(|| Bitmap::new(&flags.iter().map(|&flag| !flag).collect::<Vec<_>>(), offset)),
            prepared: partial.then(|| Bitmap::new(&ready, offset)),
            selected: Bitmap::new(&selected, offset),
            nulls: flags,
            values,
            datums,
            expected,
        }
    }
}
