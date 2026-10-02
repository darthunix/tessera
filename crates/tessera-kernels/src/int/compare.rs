//! Comparisons of two integer columns of a batch, narrowing a row mask.

use anyhow::{Result, ensure};
use tessera_core::{ColumnReader, RowMask};

use super::{IntLane, Side};
use crate::BULK_MIN_ROWS;
use crate::ops::CompareOp;

/// The driver of [`crate::int32::compare_columns`] and
/// [`crate::int64::compare_columns`].
pub(crate) fn compare_columns<T, L, R>(
    left: &L,
    right: &R,
    rows: &mut RowMask<'_>,
    op: CompareOp,
) -> Result<()>
where
    T: IntLane,
    L: ColumnReader<Value = T>,
    R: ColumnReader<Value = T>,
{
    let nrows = rows.as_view().nrows();
    ensure!(
        left.nrows() == nrows && right.nrows() == nrows,
        "column and selection row counts differ"
    );
    match op {
        CompareOp::Eq => run(left, right, rows, op, |a, b| a == b),
        CompareOp::Ne => run(left, right, rows, op, |a, b| a != b),
        CompareOp::Lt => run(left, right, rows, op, |a, b| a < b),
        CompareOp::Le => run(left, right, rows, op, |a, b| a <= b),
        CompareOp::Gt => run(left, right, rows, op, |a, b| a > b),
        CompareOp::Ge => run(left, right, rows, op, |a, b| a >= b),
    }
}

fn run<T, L, R>(
    left: &L,
    right: &R,
    rows: &mut RowMask<'_>,
    op: CompareOp,
    compare: impl Fn(T, T) -> bool,
) -> Result<()>
where
    T: IntLane,
    L: ColumnReader<Value = T>,
    R: ColumnReader<Value = T>,
{
    let nrows = rows.as_view().nrows();
    let whole_words = cfg!(all(target_arch = "aarch64", not(miri))) && nrows >= 64 && {
        let selected = rows.as_view().word_at(0);
        (selected == u64::MAX
            || (!selected.is_power_of_two() && selected.count_ones() >= BULK_MIN_ROWS))
            && blocks(left, right, 0).is_some()
    };
    if whole_words {
        return bulk(left, right, rows, op, &compare);
    }
    for index in 0..nrows.div_ceil(64) {
        let selected = rows.as_view().word_at(index);
        if selected != 0 {
            let passing = word(left, right, index, selected, &compare)?;
            rows.intersect_word(index, passing)?;
        }
    }
    Ok(())
}

/// The passing rows of one word, the columns zipped at its selected rows,
/// which the reader trait yields in the same order for both.
#[inline(always)]
fn word<T, L, R>(
    left: &L,
    right: &R,
    index: usize,
    selected: u64,
    compare: &impl Fn(T, T) -> bool,
) -> Result<u64>
where
    T: IntLane,
    L: ColumnReader<Value = T>,
    R: ColumnReader<Value = T>,
{
    let pairs = left
        .word_values(index, selected)?
        .zip(right.word_values(index, selected)?);
    Ok(pairs.fold(0, |passing, ((row, a), (_, b))| {
        let passes = matches!((a, b), (Some(a), Some(b)) if compare(a, b));
        passing | (u64::from(passes) << (row % 64))
    }))
}

/// Whole words where both columns expose them, rows elsewhere.
#[inline(never)]
fn bulk<T, L, R>(
    left: &L,
    right: &R,
    rows: &mut RowMask<'_>,
    op: CompareOp,
    compare: &impl Fn(T, T) -> bool,
) -> Result<()>
where
    T: IntLane,
    L: ColumnReader<Value = T>,
    R: ColumnReader<Value = T>,
{
    for index in 0..rows.as_view().nrows().div_ceil(64) {
        let selected = rows.as_view().word_at(index);
        if selected == 0 {
            continue;
        }
        let passing = match blocks(left, right, index) {
            Some((lhs, rhs, present)) if !selected.is_power_of_two() => {
                T::compare_sides(lhs, rhs, op) & present
            }
            _ => word(left, right, index, selected, compare)?,
        };
        rows.intersect_word(index, passing)?;
    }
    Ok(())
}

/// The storage of a whole word of both columns with their non-NULL rows.
#[cfg(all(target_arch = "aarch64", not(miri)))]
fn blocks<'a, T, L, R>(
    left: &'a L,
    right: &'a R,
    index: usize,
) -> Option<(Side<'a, T>, Side<'a, T>, u64)>
where
    T: IntLane,
    L: ColumnReader<Value = T>,
    R: ColumnReader<Value = T>,
{
    let (lhs, left_present) = T::side(left.word_block(index)?);
    let (rhs, right_present) = T::side(right.word_block(index)?);
    Some((lhs, rhs, left_present & right_present))
}

#[cfg(not(all(target_arch = "aarch64", not(miri))))]
fn blocks<'a, T, L, R>(_: &'a L, _: &'a R, _: usize) -> Option<(Side<'a, T>, Side<'a, T>, u64)>
where
    T: IntLane,
    L: ColumnReader<Value = T>,
    R: ColumnReader<Value = T>,
{
    None
}
