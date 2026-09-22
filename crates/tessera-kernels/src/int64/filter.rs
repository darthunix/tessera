//! Comparisons of selected int8 values with a scalar, narrowing a row mask.

use anyhow::{Result, ensure};
use tessera_core::{ColumnReader, RowMask};

use super::CompareOp;

/// Keep selected, non-NULL rows satisfying `column op scalar`.
///
/// Row indices remain physical; removed rows are never restored. Only
/// nonempty words are read, each at its selected rows. Column values and
/// their masks are borrowed without copying or mutation; `rows` is borrowed
/// exclusively. There are no allocations on success and no alignment
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

/// Every word at its selected rows; the loop hoists the mask decoding.
fn filter_with<C: ColumnReader<Value = i64>>(
    column: &C,
    rows: &mut RowMask<'_>,
    scalar: i64,
    compare: impl Fn(i64, i64) -> bool,
) -> Result<()> {
    for index in 0..rows.as_view().nrows().div_ceil(64) {
        let selected = rows.as_view().word(index).unwrap();
        if selected == 0 {
            continue;
        }
        let passing = if selected.is_power_of_two() {
            single_passing(column, index, selected, scalar, &compare)?
        } else {
            // fold is the word iterator's bulk path; the predicate becomes a
            // bit so that the loop has no data-dependent branch.
            column
                .word_values(index, selected)?
                .fold(0, |passing, (row, value)| {
                    let passes = value.is_some_and(|value| compare(value, scalar));
                    passing | (u64::from(passes) << (row % 64))
                })
        };
        rows.intersect_word(index, passing)?;
    }
    Ok(())
}

/// One row needs only its readiness and NULL bits, not whole mask words.
#[inline(always)]
fn single_passing<C: ColumnReader<Value = i64>>(
    column: &C,
    index: usize,
    selected: u64,
    scalar: i64,
    compare: &impl Fn(i64, i64) -> bool,
) -> Result<u64> {
    let row = index * 64 + selected.trailing_zeros() as usize;
    Ok(
        if column.get(row)?.is_some_and(|value| compare(value, scalar)) {
            selected
        } else {
            0
        },
    )
}
