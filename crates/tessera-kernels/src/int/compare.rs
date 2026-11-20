//! Comparisons of two integer columns of a batch, narrowing a row mask.

use anyhow::{Result, ensure};
use tessera_core::{ColumnReader, RowMask, WordBlock};

use super::{IntLane, Side};
use crate::WholeRows;
use crate::ops::CompareOp;
use crate::simd::Token;

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

/// The bounds of whole words of the comparison of two columns, Datum where
/// either operand's block holds Datum slots: over dense values words of 8
/// selected rows, the fewest measured, gain on both machines; over Datum
/// slots they gain from 12 on x86-64 and from 24 on AArch64. The int8
/// comparison, which no program measures, shares the int4 one's path and
/// takes its bounds.
#[cfg(target_arch = "x86_64")]
const WHOLE_ROWS: WholeRows = WholeRows {
    dense: 8,
    datum: 12,
};
#[cfg(not(target_arch = "x86_64"))]
const WHOLE_ROWS: WholeRows = WholeRows {
    dense: 8,
    datum: 24,
};

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
    let set = T::Compares::get();
    // Whether both readers expose the first word; its non-NULL rows are the
    // loop's to find.
    let whole_words = set.is_some() && nrows >= 64 && {
        let selected = rows.as_view().word(0);
        WHOLE_ROWS.may_take(selected)
            && match (left.word_block(0), right.word_block(0)) {
                (Some(lhs), Some(rhs)) => WHOLE_ROWS.takes(
                    selected,
                    matches!(lhs, WordBlock::Datum { .. })
                        || matches!(rhs, WordBlock::Datum { .. }),
                ),
                _ => false,
            }
    };
    if let Some(set) = set
        && whole_words
    {
        return bulk(set, left, right, rows, op, &compare);
    }
    for index in 0..nrows.div_ceil(64) {
        let selected = rows.as_view().word(index);
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
    set: T::Compares,
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
        let selected = rows.as_view().word(index);
        if selected == 0 {
            continue;
        }
        let passing = match blocks(set, left, right, index) {
            Some((lhs, rhs, present)) if !selected.is_power_of_two() => {
                T::compare_sides(set, lhs, rhs, op) & present
            }
            _ => word(left, right, index, selected, compare)?,
        };
        rows.intersect_word(index, passing)?;
    }
    Ok(())
}

/// The storage of a whole word of both columns with their non-NULL rows.
fn blocks<'a, T, L, R>(
    set: T::Compares,
    left: &'a L,
    right: &'a R,
    index: usize,
) -> Option<(Side<'a, T>, Side<'a, T>, u64)>
where
    T: IntLane,
    L: ColumnReader<Value = T>,
    R: ColumnReader<Value = T>,
{
    let (lhs, left_present) = T::side(set, left.word_block(index)?);
    let (rhs, right_present) = T::side(set, right.word_block(index)?);
    Some((lhs, rhs, left_present & right_present))
}
