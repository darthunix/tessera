//! Extremes of selected int8 values with PostgreSQL's NULL rules.
//!
//! NULL rows are skipped. [`min`] and [`max`] are `None` without a non-NULL
//! row. The count is the type-free [`crate::count::count`], and there is
//! no sum: PostgreSQL sums int8 into numeric, which is not a kernel. Every
//! word is read through [`ColumnReader::try_fold_selected`]: the
//! representation chooses its NULL mode once and walks full words with a
//! counted loop, and a NULL row contributes the identity of the operation
//! rather than a branch. The whole-word path follows, measured against
//! this one.

use anyhow::Result;
use tessera_core::{ColumnReader, RowMaskView};

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
    let (count, least) = column.try_fold_selected(rows, (0_usize, i64::MAX), |acc, _, value| {
        let (count, least) = acc;
        Ok((
            count + usize::from(value.is_some()),
            least.min(value.unwrap_or(i64::MAX)),
        ))
    })?;
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
    let (count, greatest) =
        column.try_fold_selected(rows, (0_usize, i64::MIN), |acc, _, value| {
            let (count, greatest) = acc;
            Ok((
                count + usize::from(value.is_some()),
                greatest.max(value.unwrap_or(i64::MIN)),
            ))
        })?;
    Ok((count > 0).then_some(greatest))
}
