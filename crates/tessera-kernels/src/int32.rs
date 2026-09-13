//! Signed int32 comparisons, without SIMD or PostgreSQL type dispatch.
//!
//! A physical int32 representation does not select PostgreSQL semantics:
//! the future caller must choose kernels by logical type and operation.

use anyhow::{Result, ensure};
use tessera_core::{ColumnReader, RowMask};

/// A comparison of a column value on the left with a non-NULL scalar on the right.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompareOp {
    /// Equal (`=`).
    Eq,
    /// Not equal (`!=`).
    Ne,
    /// Less than (`<`).
    Lt,
    /// Less than or equal (`<=`).
    Le,
    /// Greater than (`>`).
    Gt,
    /// Greater than or equal (`>=`).
    Ge,
}

/// Keep selected, non-NULL rows satisfying `column op scalar`.
///
/// Row indices remain physical; removed rows are never restored. Only selected
/// rows in nonempty words are read. Column values and their masks are borrowed
/// without copying or mutation; `rows` is borrowed exclusively. There are no
/// allocations on success and no alignment requirements beyond those of the
/// supplied reader and row mask. The operation is chosen once per call.
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
/// use tessera_kernels::int32::{CompareOp, filter};
///
/// let values = [10, 20, 30, 40];
/// let non_nulls = RowMaskView::try_new(4, &[0b1101])?;
/// let column = ColumnView::try_new(&values, Some(non_nulls))?;
/// let mut words = [0b0111];
/// let mut rows = RowMask::try_new(4, &mut words)?;
/// filter(&column, &mut rows, CompareOp::Ge, 20)?;
/// assert_eq!(rows.as_view().selected_indices().collect::<Vec<_>>(), [2]);
/// # Ok::<(), anyhow::Error>(())
/// ```
pub fn filter<C: ColumnReader<Value = i32>>(
    column: &C,
    rows: &mut RowMask<'_>,
    op: CompareOp,
    scalar: i32,
) -> Result<()> {
    ensure!(
        column.nrows() == rows.as_view().nrows(),
        "column and selection row counts differ"
    );
    match op {
        CompareOp::Eq => filter_with(column, rows, scalar, |a, b| a == b),
        CompareOp::Ne => filter_with(column, rows, scalar, |a, b| a != b),
        CompareOp::Lt => filter_with(column, rows, scalar, |a, b| a < b),
        CompareOp::Le => filter_with(column, rows, scalar, |a, b| a <= b),
        CompareOp::Gt => filter_with(column, rows, scalar, |a, b| a > b),
        CompareOp::Ge => filter_with(column, rows, scalar, |a, b| a >= b),
    }
}

fn filter_with<C: ColumnReader<Value = i32>>(
    column: &C,
    rows: &mut RowMask<'_>,
    scalar: i32,
    compare: impl Fn(i32, i32) -> bool,
) -> Result<()> {
    for index in 0..rows.as_view().nrows().div_ceil(64) {
        let selected = rows.as_view().word(index).unwrap();
        if selected == 0 {
            continue;
        }
        let mut passing = 0;
        for (row, value) in column.word_values(index, selected)? {
            if value.is_some_and(|value| compare(value, scalar)) {
                passing |= 1_u64 << (row % 64);
            }
        }
        rows.intersect_word(index, passing)?;
    }
    Ok(())
}
