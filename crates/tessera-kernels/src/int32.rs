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
/// Row indices remain physical; removed rows are never restored. Only nonempty
/// words are read. When the first word with several selected rows is full,
/// selects at least a dozen rows and the reader exposes its storage, every
/// multi-row word of the call is compared whole (vector code on AArch64) and
/// only the tail word or a word the reader refuses is read at its selected
/// rows; otherwise every word is. Column values and their masks are borrowed without
/// copying or mutation; `rows` is borrowed exclusively. There are no
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
        CompareOp::Eq => filter_with(column, rows, op, scalar, |a, b| a == b),
        CompareOp::Ne => filter_with(column, rows, op, scalar, |a, b| a != b),
        CompareOp::Lt => filter_with(column, rows, op, scalar, |a, b| a < b),
        CompareOp::Le => filter_with(column, rows, op, scalar, |a, b| a <= b),
        CompareOp::Gt => filter_with(column, rows, op, scalar, |a, b| a > b),
        CompareOp::Ge => filter_with(column, rows, op, scalar, |a, b| a >= b),
    }
}

/// Selected rows in the first multi-row word from which whole-word
/// comparisons pay for the call: on an M5 Pro a word costs 19 cycles dense
/// and 27 cycles Datum against about 2.5 cycles per selected row on the row
/// path.
const BULK_MIN_ROWS: u32 = 12;

fn filter_with<C: ColumnReader<Value = i32>>(
    column: &C,
    rows: &mut RowMask<'_>,
    op: CompareOp,
    scalar: i32,
    compare: impl Fn(i32, i32) -> bool,
) -> Result<()> {
    let nrows = rows.as_view().nrows();
    // Leading empty and single-row words are handled here; the first word
    // with several selected rows decides the strategy for the rest of the
    // call: whole-word comparisons when it is full, dense and the reader
    // exposes its storage, rows otherwise. Selectivity is roughly uniform
    // within a batch, and a partly prepared batch or a reader without bulk
    // storage refuses its first word as it would refuse the others. Deciding
    // once keeps both loops free of per-word bookkeeping, which cost more
    // than the decision on every layout tried.
    for index in 0..nrows.div_ceil(64) {
        let selected = rows.as_view().word(index).unwrap();
        if selected == 0 {
            continue;
        }
        if !selected.is_power_of_two() {
            let bulk = cfg!(all(target_arch = "aarch64", not(miri)))
                && index < nrows / 64
                && (selected == u64::MAX || selected.count_ones() >= BULK_MIN_ROWS)
                && column.word_block(index).is_some();
            return if bulk {
                filter_bulk(column, rows, index, op, scalar, &compare)
            } else {
                filter_rows(column, rows, index, scalar, &compare)
            };
        }
        let passing = single_passing(column, index, selected, scalar, &compare)?;
        rows.intersect_word(index, passing)?;
    }
    Ok(())
}

/// Every word from `first` on at its selected rows; the loop hoists the
/// mask decoding.
fn filter_rows<C: ColumnReader<Value = i32>>(
    column: &C,
    rows: &mut RowMask<'_>,
    first: usize,
    scalar: i32,
    compare: &impl Fn(i32, i32) -> bool,
) -> Result<()> {
    for index in first..rows.as_view().nrows().div_ceil(64) {
        let selected = rows.as_view().word(index).unwrap();
        if selected == 0 {
            continue;
        }
        let passing = if selected.is_power_of_two() {
            single_passing(column, index, selected, scalar, compare)?
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

/// Every multi-row word from `first` on compared whole; the tail word and
/// any word the reader refuses take the row path out of line. Once a call
/// is here, a word of even a few rows is cheaper whole than through that
/// call. Out of line so that the row loop, inlined into the entry with the
/// leading words, keeps the shape it had before the whole-word path existed.
#[inline(never)]
fn filter_bulk<C: ColumnReader<Value = i32>>(
    column: &C,
    rows: &mut RowMask<'_>,
    first: usize,
    op: CompareOp,
    scalar: i32,
    compare: &impl Fn(i32, i32) -> bool,
) -> Result<()> {
    for index in first..rows.as_view().nrows().div_ceil(64) {
        let selected = rows.as_view().word(index).unwrap();
        if selected == 0 {
            continue;
        }
        let passing = if selected.is_power_of_two() {
            single_passing(column, index, selected, scalar, compare)?
        } else if let Some(passing) = bulk_passing(column, index, op, scalar) {
            // Intersecting keeps the selection.
            passing
        } else {
            row_passing(column, index, selected, scalar, compare)?
        };
        rows.intersect_word(index, passing)?;
    }
    Ok(())
}

/// One row needs only its readiness and NULL bits, not whole mask words.
#[inline(always)]
fn single_passing<C: ColumnReader<Value = i32>>(
    column: &C,
    index: usize,
    selected: u64,
    scalar: i32,
    compare: &impl Fn(i32, i32) -> bool,
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

/// The selected rows of one word compared one by one, for the words the
/// whole-word loop cannot compare. Out of line so that its loop does not
/// share registers with that loop.
#[inline(never)]
fn row_passing<C: ColumnReader<Value = i32>>(
    column: &C,
    index: usize,
    selected: u64,
    scalar: i32,
    compare: &impl Fn(i32, i32) -> bool,
) -> Result<u64> {
    Ok(column
        .word_values(index, selected)?
        .fold(0, |passing, (row, value)| {
            let passes = value.is_some_and(|value| compare(value, scalar));
            passing | (u64::from(passes) << (row % 64))
        }))
}

/// The passing rows of a full prepared word compared with vector code, or
/// `None` where the reader exposes no storage for it or no vector code
/// exists (other targets, Miri): the word then takes the row path. Out of
/// line: inlined, its vector call spilled the loop's registers around every
/// word.
#[cfg(all(target_arch = "aarch64", not(miri)))]
#[inline(never)]
fn bulk_passing<C: ColumnReader<Value = i32>>(
    column: &C,
    index: usize,
    op: CompareOp,
    scalar: i32,
) -> Option<u64> {
    use tessera_core::WordBlock;
    Some(match column.word_block(index)? {
        WordBlock::Dense { values, non_nulls } => {
            crate::simd::filter_dense(values, scalar, op) & non_nulls
        }
        WordBlock::Datum { values, isnull } => {
            crate::simd::filter_datum(values, isnull, scalar, op)
        }
    })
}

#[cfg(not(all(target_arch = "aarch64", not(miri))))]
#[inline(never)]
fn bulk_passing<C: ColumnReader<Value = i32>>(
    _column: &C,
    _index: usize,
    _op: CompareOp,
    _scalar: i32,
) -> Option<u64> {
    None
}
