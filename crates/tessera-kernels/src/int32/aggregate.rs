//! Aggregates over selected int4 values with PostgreSQL's NULL rules.
//!
//! NULL rows are skipped; [`crate::count::count`] counts the selected
//! non-NULL rows of any type and `count(*)` is
//! [`RowMaskView::selected_count`]. [`sum`] is the int8 sum of
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
use tessera_core::{ColumnReader, RowMaskView};

/// Sum the selected non-NULL values as int8, `None` without any.
///
/// # Errors
///
/// Different row counts and unprepared selected rows fail as in
/// [`ColumnReader::try_fold_selected`]; rows already summed are not reported.
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
    let (count, total) = crate::int::aggregate(
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
/// As for [`sum`].
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
    crate::int::min(column, rows)
}

/// The greatest selected non-NULL value, `None` without any.
///
/// # Errors
///
/// As for [`sum`].
pub fn max<C: ColumnReader<Value = i32>>(
    column: &C,
    rows: &RowMaskView<'_>,
) -> Result<Option<i32>> {
    crate::int::max(column, rows)
}

/// The whole-word sum: the present rows of a word are its selected
/// non-NULL rows, and the kernel returns their count with its result.
#[cfg(all(target_arch = "aarch64", not(miri)))]
mod bulk {
    use tessera_core::WordBlock;

    use crate::int::present;
    use crate::simd;

    #[inline]
    pub fn sum(block: WordBlock<'_, i32>, selected: u64) -> (usize, i64) {
        let mask = present(&block, selected);
        let total = match block {
            WordBlock::Dense { values, .. } => simd::sum_dense(values, mask),
            WordBlock::Datum { values, .. } => simd::sum_datum(values, mask),
        };
        (mask.count_ones() as usize, total)
    }
}

/// Without vector code no call takes the whole-word path; this keeps the
/// caller compiling and is never reached.
#[cfg(not(all(target_arch = "aarch64", not(miri))))]
mod bulk {
    use tessera_core::WordBlock;

    pub fn sum(_: WordBlock<'_, i32>, _: u64) -> (usize, i64) {
        unreachable!("no whole-word kernels on this target")
    }
}
