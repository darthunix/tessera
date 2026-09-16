mod int32;

pub use int32::{DatumInt32Column, DenseInt32Column};

use anyhow::{Result, ensure};
use tessera_core::RowMaskView;

fn validate_mask(nrows: usize, mask: Option<RowMaskView<'_>>) -> Result<()> {
    ensure!(
        mask.is_none_or(|mask| mask.nrows() == nrows),
        "column and mask row counts differ"
    );
    Ok(())
}

#[inline(always)]
fn validate_ready(nrows: usize, prepared: Option<RowMaskView<'_>>, row: usize) -> Result<()> {
    ensure!(
        row < nrows,
        "physical row {row} is out of bounds for {nrows} rows"
    );
    ensure!(
        prepared.is_none_or(|mask| mask.contains(row).unwrap()),
        "physical row {row} is unprepared"
    );
    Ok(())
}

#[inline]
fn mask_word(mask: Option<RowMaskView<'_>>, word_index: usize) -> u64 {
    mask.map_or(u64::MAX, |mask| mask.word(word_index).unwrap_or(0))
}

// Shared early-exit loop over prepared selection words. The caller chooses the
// NULL mode once and passes a matching read closure; the loop validates each
// nonempty word before reading it and stops at the first reader or consumer
// error without rolling back rows already folded.
#[inline(always)]
fn try_fold_words<B, V>(
    nrows: usize,
    rows: &RowMaskView<'_>,
    prepared: Option<RowMaskView<'_>>,
    non_nulls: Option<RowMaskView<'_>>,
    mut read: impl FnMut(usize, u64) -> Option<V>,
    mut acc: B,
    mut fold: impl FnMut(B, usize, Option<V>) -> Result<B>,
) -> Result<B> {
    ensure!(
        nrows == rows.nrows(),
        "column and selection row counts differ"
    );
    for index in 0..nrows.div_ceil(64) {
        let mut remaining = rows.word(index).unwrap();
        if remaining == 0 {
            continue;
        }
        ensure!(
            remaining & !mask_word(prepared, index) == 0,
            "selection contains unprepared rows"
        );
        let bits = mask_word(non_nulls, index);
        let base = index * 64;
        while remaining != 0 {
            let row = base + remaining.trailing_zeros() as usize;
            remaining &= remaining - 1;
            acc = fold(acc, row, read(row, bits))?;
        }
    }
    Ok(acc)
}
