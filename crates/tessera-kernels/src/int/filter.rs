//! Comparisons of selected integer values with a scalar, narrowing a row
//! mask.

use anyhow::{Result, ensure};
use tessera_core::{ColumnReader, RowMask};

use super::IntLane;
use crate::BULK_MIN_ROWS;
use crate::ops::CompareOp;

/// The driver of [`crate::int32::filter`] and [`crate::int64::filter`].
pub(crate) fn filter<T: IntLane, C: ColumnReader<Value = T>>(
    column: &C,
    rows: &mut RowMask<'_>,
    op: CompareOp,
    scalar: T,
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

fn filter_with<T: IntLane, C: ColumnReader<Value = T>>(
    column: &C,
    rows: &mut RowMask<'_>,
    op: CompareOp,
    scalar: T,
    compare: impl Fn(T, T) -> bool,
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
fn filter_rows<T: IntLane, C: ColumnReader<Value = T>>(
    column: &C,
    rows: &mut RowMask<'_>,
    first: usize,
    scalar: T,
    compare: &impl Fn(T, T) -> bool,
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
fn filter_bulk<T: IntLane, C: ColumnReader<Value = T>>(
    column: &C,
    rows: &mut RowMask<'_>,
    first: usize,
    op: CompareOp,
    scalar: T,
    compare: &impl Fn(T, T) -> bool,
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
fn single_passing<T: IntLane, C: ColumnReader<Value = T>>(
    column: &C,
    index: usize,
    selected: u64,
    scalar: T,
    compare: &impl Fn(T, T) -> bool,
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
fn row_passing<T: IntLane, C: ColumnReader<Value = T>>(
    column: &C,
    index: usize,
    selected: u64,
    scalar: T,
    compare: &impl Fn(T, T) -> bool,
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
fn bulk_passing<T: IntLane, C: ColumnReader<Value = T>>(
    column: &C,
    index: usize,
    op: CompareOp,
    scalar: T,
) -> Option<u64> {
    Some(T::filter_block(column.word_block(index)?, scalar, op))
}

#[cfg(not(all(target_arch = "aarch64", not(miri))))]
#[inline(never)]
fn bulk_passing<T: IntLane, C: ColumnReader<Value = T>>(
    _column: &C,
    _index: usize,
    _op: CompareOp,
    _scalar: T,
) -> Option<u64> {
    None
}
