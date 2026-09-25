//! Comparisons of two int8 columns of a batch, narrowing a row mask.

use anyhow::{Result, ensure};
use tessera_core::{ColumnReader, RowMask};

use super::{BULK_MIN_ROWS, CompareOp, Side};

/// Keep the selected rows where both columns are non-NULL and
/// `left op right`.
///
/// Row indices remain physical; removed rows are never restored. The first
/// word with several selected rows decides the strategy, as for
/// [`super::filter`]: when it is full or selects at least a dozen rows and
/// both readers expose its storage, every multi-row word both expose is
/// compared whole (vector code on AArch64); every other word is read at its
/// selected rows, the two columns in lockstep. Nothing is allocated.
///
/// # Errors
///
/// Different row counts fail before any change. A reader error leaves the
/// current word and later ones intact and earlier ones filtered: the caller
/// discards the selection.
pub fn compare_columns<L, R>(
    left: &L,
    right: &R,
    rows: &mut RowMask<'_>,
    op: CompareOp,
) -> Result<()>
where
    L: ColumnReader<Value = i64>,
    R: ColumnReader<Value = i64>,
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

fn run<L, R>(
    left: &L,
    right: &R,
    rows: &mut RowMask<'_>,
    op: CompareOp,
    compare: impl Fn(i64, i64) -> bool,
) -> Result<()>
where
    L: ColumnReader<Value = i64>,
    R: ColumnReader<Value = i64>,
{
    let nrows = rows.as_view().nrows();
    let whole_words = cfg!(all(target_arch = "aarch64", not(miri))) && nrows >= 64 && {
        let selected = rows.as_view().word(0).unwrap();
        (selected == u64::MAX
            || (!selected.is_power_of_two() && selected.count_ones() >= BULK_MIN_ROWS))
            && blocks(left, right, 0).is_some()
    };
    if whole_words {
        return bulk(left, right, rows, op, &compare);
    }
    for index in 0..nrows.div_ceil(64) {
        let selected = rows.as_view().word(index).unwrap();
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
fn word<L, R>(
    left: &L,
    right: &R,
    index: usize,
    selected: u64,
    compare: &impl Fn(i64, i64) -> bool,
) -> Result<u64>
where
    L: ColumnReader<Value = i64>,
    R: ColumnReader<Value = i64>,
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
fn bulk<L, R>(
    left: &L,
    right: &R,
    rows: &mut RowMask<'_>,
    op: CompareOp,
    compare: &impl Fn(i64, i64) -> bool,
) -> Result<()>
where
    L: ColumnReader<Value = i64>,
    R: ColumnReader<Value = i64>,
{
    for index in 0..rows.as_view().nrows().div_ceil(64) {
        let selected = rows.as_view().word(index).unwrap();
        if selected == 0 {
            continue;
        }
        let passing = match blocks(left, right, index) {
            Some((lhs, rhs, present)) if !selected.is_power_of_two() => {
                bulk_op::compare_sides64(lhs, rhs, op) & present
            }
            _ => word(left, right, index, selected, compare)?,
        };
        rows.intersect_word(index, passing)?;
    }
    Ok(())
}

/// The storage of a whole word of both columns with their non-NULL rows.
#[cfg(all(target_arch = "aarch64", not(miri)))]
fn blocks<'a, L, R>(left: &'a L, right: &'a R, index: usize) -> Option<(Side<'a>, Side<'a>, u64)>
where
    L: ColumnReader<Value = i64>,
    R: ColumnReader<Value = i64>,
{
    use tessera_core::WordBlock;
    fn side(block: WordBlock<'_, i64>) -> (Side<'_>, u64) {
        match block {
            WordBlock::Dense { values, non_nulls } => (Side::Dense(values), non_nulls),
            WordBlock::Datum { values, isnull } => {
                (Side::Datum(values), crate::simd::non_null_bits(isnull))
            }
        }
    }
    let (lhs, left_present) = side(left.word_block(index)?);
    let (rhs, right_present) = side(right.word_block(index)?);
    Some((lhs, rhs, left_present & right_present))
}

#[cfg(not(all(target_arch = "aarch64", not(miri))))]
fn blocks<'a, L, R>(_: &'a L, _: &'a R, _: usize) -> Option<(Side<'a>, Side<'a>, u64)>
where
    L: ColumnReader<Value = i64>,
    R: ColumnReader<Value = i64>,
{
    None
}

#[cfg(all(target_arch = "aarch64", not(miri)))]
use crate::simd as bulk_op;

/// Without vector code no call takes the whole-word path.
#[cfg(not(all(target_arch = "aarch64", not(miri))))]
mod bulk_op {
    use super::{CompareOp, Side};

    pub fn compare_sides64(_: Side<'_>, _: Side<'_>, _: CompareOp) -> u64 {
        unreachable!("no whole-word kernels on this target")
    }
}
