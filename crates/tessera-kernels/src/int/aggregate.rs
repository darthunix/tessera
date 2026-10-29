//! The drivers of the aggregates over selected integer values: the least
//! and the greatest of both widths, and the loop the int4 sum shares.

use anyhow::Result;
use tessera_core::{ColumnReader, RowMaskView, WordBlock};

use super::IntLane;
use crate::BULK_MIN_ROWS;
use crate::simd::{Token, Vectors};

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
        |(count, least), set, block, selected| {
            let (present, part) = T::min_block(set, block, selected);
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
        |(count, greatest), set, block, selected| {
            let (present, part) = T::max_block(set, block, selected);
            (count + present, greatest.max(part))
        },
    )?;
    Ok((count > 0).then_some(greatest))
}

/// Run one aggregate: `fold` takes a row's value, `block` a whole word with
/// its selection and the token of the width's aggregates. The first word
/// decides the strategy for the call; deciding once keeps the loops free
/// of per-word bookkeeping. Inlined into its caller, as it was when each
/// family had its own: called, it returned the count and the result
/// through memory, 13 more instructions a call.
#[inline(always)]
pub(crate) fn aggregate<T: IntLane, C: ColumnReader<Value = T>, B: Copy>(
    column: &C,
    rows: &RowMaskView<'_>,
    init: B,
    mut fold: impl FnMut(B, Option<T>) -> B,
    block: impl for<'a> FnMut(B, T::Aggregates, WordBlock<'a, T>, u64) -> B,
) -> Result<B> {
    let set = T::Aggregates::get();
    let bulk = set.is_some()
        && rows.nrows() >= 64
        && rows.get_word(0).is_some_and(|selected| {
            (selected == u64::MAX
                || (!selected.is_power_of_two() && selected.count_ones() >= BULK_MIN_ROWS))
                && column.word_block(0).is_some()
        });
    if let Some(set) = set
        && bulk
    {
        aggregate_bulk(set, column, rows, init, fold, block)
    } else {
        column.try_fold_selected(rows, init, |acc, _, value| Ok(fold(acc, value)))
    }
}

/// Whole words where the reader exposes them; single rows through `get`,
/// the tail and refused words through the word iterator's bulk fold.
#[inline(never)]
fn aggregate_bulk<T: IntLane, C: ColumnReader<Value = T>, B: Copy>(
    set: T::Aggregates,
    column: &C,
    rows: &RowMaskView<'_>,
    init: B,
    mut fold: impl FnMut(B, Option<T>) -> B,
    mut block: impl for<'a> FnMut(B, T::Aggregates, WordBlock<'a, T>, u64) -> B,
) -> Result<B> {
    let mut acc = init;
    // On x86-64 one test for empty and single-row words, told apart inside:
    // a `continue` for empty words became a loop of its own, and every
    // other word came back through its head, 8 instructions a word. On
    // AArch64 the one test lost on the words read row by row, so there each
    // has its own.
    #[cfg(target_arch = "x86_64")]
    for (index, selected) in rows.words().enumerate() {
        if selected & selected.wrapping_sub(1) == 0 {
            if selected != 0 {
                let row = index * 64 + selected.trailing_zeros() as usize;
                acc = fold(acc, column.get(row)?);
            }
        } else if let Some(word) = column.word_block(index) {
            acc = block(acc, set, word, selected);
        } else {
            acc = column
                .word_values(index, selected)?
                .fold(acc, |acc, (_, value)| fold(acc, value));
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    for index in 0..rows.nrows().div_ceil(64) {
        let selected = rows.word(index);
        if selected == 0 {
            continue;
        }
        if selected.is_power_of_two() {
            let row = index * 64 + selected.trailing_zeros() as usize;
            acc = fold(acc, column.get(row)?);
        } else if let Some(word) = column.word_block(index) {
            acc = block(acc, set, word, selected);
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
#[inline(always)]
pub(crate) fn present<S: Vectors, T>(set: S, block: &WordBlock<'_, T>, selected: u64) -> u64 {
    match block {
        WordBlock::Dense { non_nulls, .. } => selected & non_nulls,
        WordBlock::Datum { isnull, .. } => selected & set.non_null_bits(isnull),
    }
}
