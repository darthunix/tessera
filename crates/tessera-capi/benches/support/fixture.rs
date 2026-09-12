//! Deterministic inputs shared by all measured readers and the scalar reference.
//!
//! Build dense and Datum buffers, selection/NULL/readiness masks, and an expected
//! sum before timing. The full matrix covers boundary sizes and mask layouts;
//! quick cases are a representative subset, not a replacement for that matrix.
//! All fixture buffers are initialized, even NULL and unprepared positions:
//! uninitialized-buffer safety belongs to the library's correctness/Miri tests.

use super::reference::{Bits, Mask};
use std::mem::MaybeUninit;
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
    pub dense: Vec<MaybeUninit<i32>>,
    pub datums: Vec<u64>,
    pub datum_values: Vec<MaybeUninit<u64>>,
    pub nulls: Vec<bool>,
    pub isnull: Vec<MaybeUninit<bool>>,
    pub selected: Bitmap,
    pub prepared: Option<Bitmap>,
    pub non_nulls: Option<Bitmap>,
    pub expected: i64,
}

impl Fixture {
    fn new(nrows: usize, pattern: &str, nulls: &str, offset: Option<usize>, partial: bool) -> Self {
        let ready: Vec<_> = (0..nrows).map(|row| !partial || row % 3 != 2).collect();
        let selected: Vec<_> = (0..nrows)
            .map(|row| {
                ready[row]
                    && match pattern {
                        "all" => true,
                        "half" => row % 2 == 0,
                        "sparse" => row % 64 == 0,
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
        let values: Vec<_> = (0..nrows)
            .map(|row| (row as i32).wrapping_mul(7919).wrapping_sub(104729))
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
            quick: nrows == 1024
                && matches!(
                    (pattern, nulls, offset, partial),
                    ("all", "none" | "mixed" | "all", None, false)
                        | ("sparse" | "empty", "none", None, false)
                        | ("all", "mixed", None, true)
                        | ("sparse", "mixed", Some(7), true)
                ),
            dense: values.iter().copied().map(MaybeUninit::new).collect(),
            datum_values: datums.iter().copied().map(MaybeUninit::new).collect(),
            isnull: flags.iter().copied().map(MaybeUninit::new).collect(),
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

pub fn cases() -> Vec<Fixture> {
    let mut cases = Vec::new();
    for nulls in ["none", "mixed"] {
        for pattern in ["all", "half", "sparse", "empty"] {
            cases.push(Fixture::new(1024, pattern, nulls, None, false));
        }
    }
    for nrows in [0, 1, 63, 64, 65] {
        cases.push(Fixture::new(nrows, "all", "mixed", None, false));
    }
    cases.push(Fixture::new(1024, "all", "all", None, false));
    cases.push(Fixture::new(1024, "all", "mixed", None, true));
    for (nrows, offset, pattern) in [(65, 3, "all"), (1024, 7, "all"), (1024, 7, "sparse")] {
        cases.push(Fixture::new(nrows, pattern, "mixed", Some(offset), true));
    }
    cases
}
