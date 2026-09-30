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
    crate::int::min(column, rows)
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
    crate::int::max(column, rows)
}
