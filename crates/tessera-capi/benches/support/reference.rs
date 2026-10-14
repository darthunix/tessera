//! Independent scalar reference, intentionally not using Tessera readers or
//! bitmap operations. Read and sum directly from dense or Datum buffers to
//! measure abstraction overhead against simple loops over the same data.
//! Fixture expectations also check these loops before timing. This is a timed
//! comparison implementation, not a saved run; changes invalidate old baselines.

use anyhow::{Result, ensure};

#[derive(Clone, Copy)]
pub struct Mask<'a> {
    pub nrows: usize,
    pub words: &'a [u64],
}

impl Mask<'_> {
    #[inline]
    pub fn contains(self, row: usize) -> bool {
        assert!(row < self.nrows);
        self.words[row / 64] & (1 << (row % 64)) != 0
    }

    #[inline]
    pub fn word(self, index: usize) -> u64 {
        assert!(index < self.nrows.div_ceil(64));
        self.words[index]
    }
}

// Inline the independent loop into the uniform, non-inlined timed entry point.
#[inline(always)]
pub fn dense(
    values: &[i32],
    rows: Mask<'_>,
    prepared: Option<Mask<'_>>,
    non_nulls: Option<Mask<'_>>,
) -> Result<i64> {
    ensure!(values.len() == rows.nrows, "row counts differ");
    let mut sum = 0;
    for index in 0..rows.nrows.div_ceil(64) {
        let mut selected = rows.word(index);
        if selected == 0 {
            continue;
        }
        if let Some(prepared) = prepared {
            ensure!(selected & !prepared.word(index) == 0, "unprepared rows");
        }
        let non_nulls = non_nulls.map(|mask| mask.word(index));
        while selected != 0 {
            let bit = selected.trailing_zeros() as usize;
            selected &= selected - 1;
            if non_nulls.is_none_or(|bits| bits & (1 << bit) != 0) {
                sum += i64::from(values[index * 64 + bit]);
            }
        }
    }
    Ok(sum)
}

#[inline(always)]
pub fn datum(
    values: &[u64],
    isnull: &[bool],
    rows: Mask<'_>,
    prepared: Option<Mask<'_>>,
) -> Result<i64> {
    ensure!(values.len() == rows.nrows, "row counts differ");
    let mut sum = 0;
    for index in 0..rows.nrows.div_ceil(64) {
        let mut selected = rows.word(index);
        if selected == 0 {
            continue;
        }
        if let Some(prepared) = prepared {
            ensure!(selected & !prepared.word(index) == 0, "unprepared rows");
        }
        while selected != 0 {
            let row = index * 64 + selected.trailing_zeros() as usize;
            selected &= selected - 1;
            if !isnull[row] {
                sum += i64::from(values[row] as i32);
            }
        }
    }
    Ok(sum)
}
