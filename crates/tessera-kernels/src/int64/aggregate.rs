//! Extremes of selected int8 values with PostgreSQL's NULL rules.
//!
//! NULL rows are skipped. [`min`] and [`max`] are `None` without a non-NULL
//! row. The count is the type-free [`crate::count::count`], and there is
//! no sum: PostgreSQL sums int8 into numeric, which is not a kernel.
//!
//! When the first word of the selection is full, selects at least a dozen
//! rows and the reader exposes its storage, the call aggregates whole words
//! (vector code on AArch64) and reads only single-row words, the tail and
//! refused words row by row. Otherwise it reads through
//! [`ColumnReader::try_fold_selected`]: the representation chooses its NULL
//! mode once and walks full words with a counted loop. Either way a NULL
//! row contributes the identity of the operation rather than a branch.

use anyhow::Result;
use tessera_core::{ColumnReader, RowMaskView, WordBlock};

use super::BULK_MIN_ROWS;

/// The least selected non-NULL value, `None` without any.
///
/// # Errors
///
/// Different row counts and unprepared selected rows fail as in
/// [`ColumnReader::try_fold_selected`].
///
/// ```
/// use tessera_core::{ColumnView, RowMaskView};
/// use tessera_kernels::int64::{max, min};
///
/// let values = [10, -(1 << 40), 30, 1 << 40];
/// let non_nulls = RowMaskView::try_new(4, &[0b1101])?;
/// let column = ColumnView::try_new(&values, Some(non_nulls))?;
/// let rows = RowMaskView::try_new(4, &[0b1111])?;
/// assert_eq!(min(&column, &rows)?, Some(10));
/// assert_eq!(max(&column, &rows)?, Some(1 << 40));
/// # Ok::<(), anyhow::Error>(())
/// ```
pub fn min<C: ColumnReader<Value = i64>>(
    column: &C,
    rows: &RowMaskView<'_>,
) -> Result<Option<i64>> {
    // A NULL row contributes the identity of the operation.
    let (count, least) = aggregate(
        column,
        rows,
        (0_usize, i64::MAX),
        |(count, least), value| {
            (
                count + usize::from(value.is_some()),
                least.min(value.unwrap_or(i64::MAX)),
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
/// As for [`min`].
pub fn max<C: ColumnReader<Value = i64>>(
    column: &C,
    rows: &RowMaskView<'_>,
) -> Result<Option<i64>> {
    let (count, greatest) = aggregate(
        column,
        rows,
        (0_usize, i64::MIN),
        |(count, greatest), value| {
            (
                count + usize::from(value.is_some()),
                greatest.max(value.unwrap_or(i64::MIN)),
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
/// its selection. The first word decides the strategy for the call, as for
/// the int4 aggregates.
fn aggregate<C: ColumnReader<Value = i64>, B: Copy>(
    column: &C,
    rows: &RowMaskView<'_>,
    init: B,
    mut fold: impl FnMut(B, Option<i64>) -> B,
    block: impl for<'a> FnMut(B, WordBlock<'a, i64>, u64) -> B,
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
fn aggregate_bulk<C: ColumnReader<Value = i64>, B: Copy>(
    column: &C,
    rows: &RowMaskView<'_>,
    init: B,
    mut fold: impl FnMut(B, Option<i64>) -> B,
    mut block: impl for<'a> FnMut(B, WordBlock<'a, i64>, u64) -> B,
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
    fn present(block: &WordBlock<'_, i64>, selected: u64) -> u64 {
        match block {
            WordBlock::Dense { non_nulls, .. } => selected & non_nulls,
            WordBlock::Datum { isnull, .. } => selected & simd::non_null_bits(isnull),
        }
    }

    #[inline]
    pub fn min(block: WordBlock<'_, i64>, selected: u64) -> (usize, i64) {
        let mask = present(&block, selected);
        let least = match block {
            WordBlock::Dense { values, .. } => simd::min_dense64(values, mask),
            WordBlock::Datum { values, .. } => simd::min_datum64(values, mask),
        };
        (mask.count_ones() as usize, least)
    }

    #[inline]
    pub fn max(block: WordBlock<'_, i64>, selected: u64) -> (usize, i64) {
        let mask = present(&block, selected);
        let greatest = match block {
            WordBlock::Dense { values, .. } => simd::max_dense64(values, mask),
            WordBlock::Datum { values, .. } => simd::max_datum64(values, mask),
        };
        (mask.count_ones() as usize, greatest)
    }
}

/// Without vector code no call takes the whole-word path; these keep the
/// callers compiling and are never reached.
#[cfg(not(all(target_arch = "aarch64", not(miri))))]
mod bulk {
    use tessera_core::WordBlock;

    pub fn min(_: WordBlock<'_, i64>, _: u64) -> (usize, i64) {
        unreachable!("no whole-word kernels on this target")
    }

    pub fn max(_: WordBlock<'_, i64>, _: u64) -> (usize, i64) {
        unreachable!("no whole-word kernels on this target")
    }
}
