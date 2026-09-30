//! Comparisons of selected int8 values with a scalar, narrowing a row mask.

use anyhow::Result;
use tessera_core::{ColumnReader, RowMask};

use super::CompareOp;

/// Keep selected, non-NULL rows satisfying `column op scalar`.
///
/// Row indices remain physical; removed rows are never restored. Only nonempty
/// words are read. When the first word with several selected rows is full,
/// selects at least a dozen rows and the reader exposes its storage, every
/// multi-row word of the call is compared whole (vector code on AArch64) and
/// only the tail word or a word the reader refuses is read at its selected
/// rows; otherwise every word is. Column values and their masks are borrowed
/// without copying or mutation; `rows` is borrowed exclusively. There are no allocations on success and no alignment
/// requirements beyond those of the supplied reader and row mask. The
/// operation is chosen once per call.
///
/// # Errors
///
/// Different row counts fail before any mutation. A reader error (including an
/// unprepared selected row) leaves the current word and all later words intact.
/// Earlier words remain filtered: there is no rollback, and the caller must
/// discard the partial selection after an error rather than use it as a result.
/// Empty selections are valid and do not require their rows to be prepared.
///
/// ```
/// use tessera_core::{ColumnView, RowMask, RowMaskView};
/// use tessera_kernels::int64::{CompareOp, filter};
///
/// let values = [10, 20, 30, 1 << 40];
/// let non_nulls = RowMaskView::try_new(4, &[0b1101])?;
/// let column = ColumnView::try_new(&values, Some(non_nulls))?;
/// let mut words = [0b1111];
/// let mut rows = RowMask::try_new(4, &mut words)?;
/// filter(&column, &mut rows, CompareOp::Ge, 20)?;
/// assert_eq!(rows.as_view().selected_indices().collect::<Vec<_>>(), [2, 3]);
/// # Ok::<(), anyhow::Error>(())
/// ```
pub fn filter<C: ColumnReader<Value = i64>>(
    column: &C,
    rows: &mut RowMask<'_>,
    op: CompareOp,
    scalar: i64,
) -> Result<()> {
    crate::int::filter(column, rows, op, scalar)
}
