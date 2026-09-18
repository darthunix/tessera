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
//! The kernels read through [`ColumnReader::try_fold_selected`]: the
//! representation chooses its NULL mode once and walks full words with a
//! counted loop, and the folds carry a NULL row as an identity value rather
//! than a branch, so the accumulator's dependency chain stays short.

use anyhow::Result;
use tessera_core::{ColumnReader, RowMaskView};

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
    column.try_fold_selected(rows, 0, |count, _, value| {
        Ok(count + usize::from(value.is_some()))
    })
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
    let (count, total) =
        column.try_fold_selected(rows, (0_usize, 0_i64), |(count, total), _, value| {
            Ok((
                count + usize::from(value.is_some()),
                total + value.map_or(0, i64::from),
            ))
        })?;
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
    let (count, least) =
        column.try_fold_selected(rows, (0_usize, i32::MAX), |(count, least), _, value| {
            Ok((
                count + usize::from(value.is_some()),
                least.min(value.unwrap_or(i32::MAX)),
            ))
        })?;
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
    let (count, greatest) =
        column.try_fold_selected(rows, (0_usize, i32::MIN), |(count, greatest), _, value| {
            Ok((
                count + usize::from(value.is_some()),
                greatest.max(value.unwrap_or(i32::MIN)),
            ))
        })?;
    Ok((count > 0).then_some(greatest))
}
