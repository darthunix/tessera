//! Aggregates over selected int4 values with PostgreSQL's NULL rules.
//!
//! NULL rows are skipped. [`count`] is the number of selected non-NULL rows;
//! `count(*)` is [`RowMaskView::selected_count`]. [`sum`] is the int8 sum of
//! the int4 values and `None` without a non-NULL row; within one call it
//! cannot overflow (at most 2^31 rows of magnitude at most 2^31), so overflow
//! checking belongs to whoever adds calls together, where PostgreSQL raises
//! `bigint out of range`. [`min`] and [`max`] are `None` without a non-NULL
//! row.
//!
//! When the first word of the selection is full, selects at least a dozen
//! rows and the reader exposes its storage, the call aggregates whole words
//! (vector code on AArch64) and reads only single-row words, the tail and
//! refused words row by row. Otherwise it reads through
//! [`ColumnReader::try_fold_selected`]: the representation chooses its NULL
//! mode once and walks full words with a counted loop. Only the first word is
//! consulted: selectivity is roughly uniform within a batch, and scanning for
//! a first nonempty word cost an empty selection almost as much as reading
//! it. Either way a NULL row contributes an identity value rather than a
//! branch, so the accumulator's dependency chain stays short.

use anyhow::Result;
use tessera_core::{ColumnReader, RowMaskView, WordBlock};

use super::BULK_MIN_ROWS;

/// Count the selected non-NULL rows.
///
/// # Errors
///
/// Different row counts and unprepared selected rows fail as in
/// [`ColumnReader::try_fold_selected`]; rows already counted are not reported.
///
/// ```
/// use tessera_core::{ColumnView, RowMaskView};
/// use tessera_kernels::int32::count;
///
/// let values = [10, 20, 30, 40];
/// let non_nulls = RowMaskView::try_new(4, &[0b1101])?;
/// let column = ColumnView::try_new(&values, Some(non_nulls))?;
/// let rows = RowMaskView::try_new(4, &[0b0111])?;
/// assert_eq!(count(&column, &rows)?, 2);
/// # Ok::<(), anyhow::Error>(())
/// ```
pub fn count<C: ColumnReader<Value = i32>>(column: &C, rows: &RowMaskView<'_>) -> Result<usize> {
    aggregate(
        column,
        rows,
        0,
        |count, value| count + usize::from(value.is_some()),
        |count, block, selected| count + bulk::count(block, selected),
    )
}

/// Sum the selected non-NULL values as int8, `None` without any.
///
/// # Errors
///
/// As for [`count`].
///
/// ```
/// use tessera_core::{ColumnView, RowMaskView};
/// use tessera_kernels::int32::sum;
///
/// let values = [i32::MAX, 20, 30, i32::MAX];
/// let column = ColumnView::try_new(&values, None)?;
/// let rows = RowMaskView::try_new(4, &[0b1001])?;
/// assert_eq!(sum(&column, &rows)?, Some(2 * i64::from(i32::MAX)));
/// assert_eq!(sum(&column, &RowMaskView::try_new(4, &[0])?)?, None);
/// # Ok::<(), anyhow::Error>(())
/// ```
pub fn sum<C: ColumnReader<Value = i32>>(
    column: &C,
    rows: &RowMaskView<'_>,
) -> Result<Option<i64>> {
    let (count, total) = aggregate(
        column,
        rows,
        (0_usize, 0_i64),
        |(count, total), value| {
            (
                count + usize::from(value.is_some()),
                total + value.map_or(0, i64::from),
            )
        },
        |(count, total), block, selected| {
            let (present, part) = bulk::sum(block, selected);
            (count + present, total + part)
        },
    )?;
    Ok((count > 0).then_some(total))
}

/// The least selected non-NULL value, `None` without any.
///
/// # Errors
///
/// As for [`count`].
///
/// ```
/// use tessera_core::{ColumnView, RowMaskView};
/// use tessera_kernels::int32::{max, min};
///
/// let values = [10, -20, 30, 40];
/// let non_nulls = RowMaskView::try_new(4, &[0b1101])?;
/// let column = ColumnView::try_new(&values, Some(non_nulls))?;
/// let rows = RowMaskView::try_new(4, &[0b0111])?;
/// assert_eq!(min(&column, &rows)?, Some(10));
/// assert_eq!(max(&column, &rows)?, Some(30));
/// # Ok::<(), anyhow::Error>(())
/// ```
pub fn min<C: ColumnReader<Value = i32>>(
    column: &C,
    rows: &RowMaskView<'_>,
) -> Result<Option<i32>> {
    // A NULL row contributes the identity of the operation.
    let (count, least) = aggregate(
        column,
        rows,
        (0_usize, i32::MAX),
        |(count, least), value| {
            (
                count + usize::from(value.is_some()),
                least.min(value.unwrap_or(i32::MAX)),
            )
        },
        |(count, least), block, selected| {
            let (present, part) = bulk::min(block, selected);
            (count + present, least.min(part))
        },
    )?;
    Ok((count > 0).then_some(least))
}

/// The greatest selected non-NULL value, `None` without any.
///
/// # Errors
///
/// As for [`count`].
pub fn max<C: ColumnReader<Value = i32>>(
    column: &C,
    rows: &RowMaskView<'_>,
) -> Result<Option<i32>> {
    let (count, greatest) = aggregate(
        column,
        rows,
        (0_usize, i32::MIN),
        |(count, greatest), value| {
            (
                count + usize::from(value.is_some()),
                greatest.max(value.unwrap_or(i32::MIN)),
            )
        },
        |(count, greatest), block, selected| {
            let (present, part) = bulk::max(block, selected);
            (count + present, greatest.max(part))
        },
    )?;
    Ok((count > 0).then_some(greatest))
}

/// Run one aggregate: `fold` takes a row's value, `block` a whole word with
/// its selection. The first word decides the strategy for the call; deciding
/// once keeps the loops free of per-word bookkeeping.
fn aggregate<C: ColumnReader<Value = i32>, B: Copy>(
    column: &C,
    rows: &RowMaskView<'_>,
    init: B,
    mut fold: impl FnMut(B, Option<i32>) -> B,
    block: impl for<'a> FnMut(B, WordBlock<'a, i32>, u64) -> B,
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
fn aggregate_bulk<C: ColumnReader<Value = i32>, B: Copy>(
    column: &C,
    rows: &RowMaskView<'_>,
    init: B,
    mut fold: impl FnMut(B, Option<i32>) -> B,
    mut block: impl for<'a> FnMut(B, WordBlock<'a, i32>, u64) -> B,
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

/// Whole-word kernels: the present rows of a word are its selected non-NULL
/// rows, and each kernel returns their count with its result. Inlined into
/// the generic loop so that the block stays in registers.
#[cfg(all(target_arch = "aarch64", not(miri)))]
mod bulk {
    use tessera_core::WordBlock;

    use crate::simd;

    #[inline(always)]
    fn present(block: &WordBlock<'_, i32>, selected: u64) -> u64 {
        match block {
            WordBlock::Dense { non_nulls, .. } => selected & non_nulls,
            WordBlock::Datum { isnull, .. } => selected & simd::non_null_bits(isnull),
        }
    }

    #[inline]
    pub fn count(block: WordBlock<'_, i32>, selected: u64) -> usize {
        match block {
            WordBlock::Dense { non_nulls, .. } => (selected & non_nulls).count_ones() as usize,
            WordBlock::Datum { isnull, .. } => simd::count_datum(isnull, selected),
        }
    }

    #[inline]
    pub fn sum(block: WordBlock<'_, i32>, selected: u64) -> (usize, i64) {
        let mask = present(&block, selected);
        let total = match block {
            WordBlock::Dense { values, .. } => simd::sum_dense(values, mask),
            WordBlock::Datum { values, .. } => simd::sum_datum(values, mask),
        };
        (mask.count_ones() as usize, total)
    }

    #[inline]
    pub fn min(block: WordBlock<'_, i32>, selected: u64) -> (usize, i32) {
        let mask = present(&block, selected);
        let least = match block {
            WordBlock::Dense { values, .. } => simd::min_dense(values, mask),
            WordBlock::Datum { values, .. } => simd::min_datum(values, mask),
        };
        (mask.count_ones() as usize, least)
    }

    #[inline]
    pub fn max(block: WordBlock<'_, i32>, selected: u64) -> (usize, i32) {
        let mask = present(&block, selected);
        let greatest = match block {
            WordBlock::Dense { values, .. } => simd::max_dense(values, mask),
            WordBlock::Datum { values, .. } => simd::max_datum(values, mask),
        };
        (mask.count_ones() as usize, greatest)
    }
}

/// Without vector code no call takes the whole-word path; these keep the
/// callers compiling and are never reached.
#[cfg(not(all(target_arch = "aarch64", not(miri))))]
mod bulk {
    use tessera_core::WordBlock;

    pub fn count(_: WordBlock<'_, i32>, _: u64) -> usize {
        unreachable!("no whole-word kernels on this target")
    }

    pub fn sum(_: WordBlock<'_, i32>, _: u64) -> (usize, i64) {
        unreachable!("no whole-word kernels on this target")
    }

    pub fn min(_: WordBlock<'_, i32>, _: u64) -> (usize, i32) {
        unreachable!("no whole-word kernels on this target")
    }

    pub fn max(_: WordBlock<'_, i32>, _: u64) -> (usize, i32) {
        unreachable!("no whole-word kernels on this target")
    }
}
