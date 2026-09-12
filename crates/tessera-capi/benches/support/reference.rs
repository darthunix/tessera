//! Independent scalar reference, intentionally not using Tessera readers or
//! bitmap operations. Read and sum directly from dense or Datum buffers to
//! measure abstraction overhead against simple loops over the same data.
//! Fixture expectations also check these loops before timing. This is a timed
//! comparison implementation, not a saved run; changes invalidate old baselines.

use anyhow::{Result, ensure};

#[derive(Clone, Copy)]
pub enum Bits<'a> {
    Words(&'a [u64]),
    Bytes(&'a [u8], usize),
}

#[derive(Clone, Copy)]
pub struct Mask<'a> {
    pub nrows: usize,
    pub bits: Bits<'a>,
}

impl Mask<'_> {
    #[inline]
    pub fn word(self, index: usize) -> u64 {
        assert!(index < self.nrows.div_ceil(64));
        match self.bits {
            Bits::Words(words) => words[index],
            Bits::Bytes(bytes, offset) => byte_word(bytes, offset, index, self.nrows),
        }
    }
}

#[inline(never)]
fn byte_word(bytes: &[u8], offset: usize, index: usize, nrows: usize) -> u64 {
    let first_bit = offset + index * 64;
    let first_byte = first_bit / 8;
    let width = (nrows - index * 64).min(64);
    let count = (width + first_bit % 8).div_ceil(8);
    let mut buffer = [0_u8; 16];
    buffer[..count].copy_from_slice(&bytes[first_byte..first_byte + count]);
    let shifted = u128::from_le_bytes(buffer) >> (first_bit % 8);
    shifted as u64 & (u64::MAX >> (64 - width))
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
