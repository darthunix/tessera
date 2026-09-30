//! The drivers of the aggregates over selected integer values: the least
//! and the greatest of both widths, and the loop the int4 sum shares.

use anyhow::Result;
use tessera_core::{ColumnReader, RowMaskView, WordBlock};

use super::IntLane;
use crate::BULK_MIN_ROWS;

/// The driver of [`crate::int32::min`] and [`crate::int64::min`].
pub(crate) fn min<T: IntLane, C: ColumnReader<Value = T>>(
    column: &C,
    rows: &RowMaskView<'_>,
) -> Result<Option<T>> {
    // A NULL row contributes the identity of the operation.
    let (count, least) = aggregate(
        column,
        rows,
        (0_usize, T::MAX),
        |(count, least), value| {
            (
                count + usize::from(value.is_some()),
                least.min(value.unwrap_or(T::MAX)),
            )
        },
        |(count, least), block, selected| {
            let (present, part) = T::min_block(block, selected);
            (count + present, least.min(part))
        },
    )?;
    Ok((count > 0).then_some(least))
}

/// The driver of [`crate::int32::max`] and [`crate::int64::max`].
pub(crate) fn max<T: IntLane, C: ColumnReader<Value = T>>(
    column: &C,
    rows: &RowMaskView<'_>,
) -> Result<Option<T>> {
    let (count, greatest) = aggregate(
        column,
        rows,
        (0_usize, T::MIN),
        |(count, greatest), value| {
            (
                count + usize::from(value.is_some()),
                greatest.max(value.unwrap_or(T::MIN)),
            )
        },
        |(count, greatest), block, selected| {
            let (present, part) = T::max_block(block, selected);
            (count + present, greatest.max(part))
        },
    )?;
    Ok((count > 0).then_some(greatest))
}

/// Run one aggregate: `fold` takes a row's value, `block` a whole word with
/// its selection. The first word decides the strategy for the call; deciding
/// once keeps the loops free of per-word bookkeeping.
pub(crate) fn aggregate<T: IntLane, C: ColumnReader<Value = T>, B: Copy>(
    column: &C,
    rows: &RowMaskView<'_>,
    init: B,
    mut fold: impl FnMut(B, Option<T>) -> B,
    block: impl for<'a> FnMut(B, WordBlock<'a, T>, u64) -> B,
) -> Result<B> {
    let bulk = cfg!(all(target_arch = "aarch64", not(miri)))
        && rows.nrows() >= 64
        && rows.word(0).is_some_and(|selected| {
            (selected == u64::MAX
                || (!selected.is_power_of_two() && selected.count_ones() >= BULK_MIN_ROWS))
                && column.word_block(0).is_some()
        });
    if bulk {
        aggregate_bulk(column, rows, init, fold, block)
    } else {
        column.try_fold_selected(rows, init, |acc, _, value| Ok(fold(acc, value)))
    }
}

/// Whole words where the reader exposes them; single rows through `get`,
/// the tail and refused words through the word iterator's bulk fold.
#[inline(never)]
fn aggregate_bulk<T: IntLane, C: ColumnReader<Value = T>, B: Copy>(
    column: &C,
    rows: &RowMaskView<'_>,
    init: B,
    mut fold: impl FnMut(B, Option<T>) -> B,
    mut block: impl for<'a> FnMut(B, WordBlock<'a, T>, u64) -> B,
) -> Result<B> {
    let mut acc = init;
    for index in 0..rows.nrows().div_ceil(64) {
        let selected = rows.word(index).unwrap();
        if selected == 0 {
            continue;
        }
        if selected.is_power_of_two() {
            let row = index * 64 + selected.trailing_zeros() as usize;
            acc = fold(acc, column.get(row)?);
        } else if let Some(word) = column.word_block(index) {
            acc = block(acc, word, selected);
        } else {
            acc = column
                .word_values(index, selected)?
                .fold(acc, |acc, (_, value)| fold(acc, value));
        }
    }
    Ok(acc)
}

/// The present rows of a whole word, its selected non-NULL rows, for the
/// widths' whole-word kernels. Inlined into them so that the block stays in
/// registers.
#[cfg(all(target_arch = "aarch64", not(miri)))]
#[inline(always)]
pub(crate) fn present<T>(block: &WordBlock<'_, T>, selected: u64) -> u64 {
    match block {
        WordBlock::Dense { non_nulls, .. } => selected & non_nulls,
        WordBlock::Datum { isnull, .. } => selected & crate::simd::non_null_bits(isnull),
    }
}
