//! The driver of the arithmetic of both widths: the operand shapes, the
//! row loops with the NULL placeholder pair, the choice between whole words
//! and rows and the loop over whole words, whose operation is the width's
//! ([`IntLane::arith_block`]). The semantics are the widths' ([`crate::int32`]
//! and [`crate::int64`] arithmetic).

use std::mem::MaybeUninit;

use anyhow::{Context, Result, ensure};
use tessera_core::{ColumnReader, RowMask, RowMaskView};

use super::{IntLane, Side};
use crate::BULK_MIN_ROWS;
use crate::ops::{ArithOp, ArithmeticError};

/// The driver of `arith_scalar`: `column op scalar`.
pub(crate) fn arith_scalar<T: IntLane, C: ColumnReader<Value = T>>(
    op: ArithOp,
    column: &C,
    scalar: T,
    rows: &RowMaskView<'_>,
    values: &mut [MaybeUninit<T>],
    non_nulls: &mut RowMask<'_>,
) -> Result<()> {
    let operands: Operands<'_, T, C, C> = Operands::ColumnScalar(column, scalar);
    run(op, operands, rows, values, non_nulls)
}

/// The driver of `arith_scalar_left`: `scalar op column`.
pub(crate) fn arith_scalar_left<T: IntLane, C: ColumnReader<Value = T>>(
    op: ArithOp,
    scalar: T,
    column: &C,
    rows: &RowMaskView<'_>,
    values: &mut [MaybeUninit<T>],
    non_nulls: &mut RowMask<'_>,
) -> Result<()> {
    let operands: Operands<'_, T, C, C> = Operands::ScalarColumn(scalar, column);
    run(op, operands, rows, values, non_nulls)
}

/// The driver of `arith_columns`: `left op right`.
pub(crate) fn arith_columns<T, L, R>(
    op: ArithOp,
    left: &L,
    right: &R,
    rows: &RowMaskView<'_>,
    values: &mut [MaybeUninit<T>],
    non_nulls: &mut RowMask<'_>,
) -> Result<()>
where
    T: IntLane,
    L: ColumnReader<Value = T>,
    R: ColumnReader<Value = T>,
{
    run(op, Operands::Columns(left, right), rows, values, non_nulls)
}

/// The operands of one call.
enum Operands<'a, T, L, R> {
    ColumnScalar(&'a L, T),
    ScalarColumn(T, &'a R),
    Columns(&'a L, &'a R),
}

impl<T, L, R> Operands<'_, T, L, R>
where
    T: IntLane,
    L: ColumnReader<Value = T>,
    R: ColumnReader<Value = T>,
{
    fn column_rows(&self) -> [Option<usize>; 2] {
        match self {
            Self::ColumnScalar(left, _) => [Some(left.nrows()), None],
            Self::ScalarColumn(_, right) => [None, Some(right.nrows())],
            Self::Columns(left, right) => [Some(left.nrows()), Some(right.nrows())],
        }
    }

    /// Every word row by row. Out of line, like the whole-word loop: sharing
    /// a function with the other path's call cost this loop registers, and
    /// with them 5-20% of its instructions.
    #[inline(never)]
    fn rows<E: Evaluate<T>>(&self, output: Output<'_, '_, '_, '_, T>, evaluate: &E) -> Result<()> {
        let rows = output.rows;
        let mut output = output;
        for index in 0..rows.nrows().div_ceil(64) {
            let selected = rows.word_at(index);
            self.word(index, selected, &mut output, evaluate)?;
        }
        Ok(())
    }

    /// One word row by row. The three operand shapes give three loops of
    /// one body; two columns are zipped word by word, which the trait
    /// guarantees to yield the same rows in the same order. A NULL row is
    /// computed from the placeholder pair (0, 1), whatever the other operand
    /// holds: 0 op 1 fails in no operation, so the loop has no branch on
    /// nullness, and only the error check branches, which a NULL row never
    /// takes (`MAX + NULL` is NULL, not an overflow). The pair is built
    /// from two scalars, not an `Option` of a tuple, which the compiler kept
    /// on the stack.
    #[inline(always)]
    fn word<E: Evaluate<T>>(
        &self,
        index: usize,
        selected: u64,
        output: &mut Output<'_, '_, '_, '_, T>,
        evaluate: &E,
    ) -> Result<()> {
        if selected == 0 {
            return output.non_nulls.set_word(index, 0);
        }
        let mut present = 0;
        match self {
            Self::ColumnScalar(left, scalar) => {
                for (row, value) in left.word_values(index, selected)? {
                    let some = value.is_some();
                    let b = if some { *scalar } else { T::ONE };
                    output.values[row].write(evaluate(value.unwrap_or(T::ZERO), b)?);
                    present |= u64::from(some) << (row % 64);
                }
            }
            Self::ScalarColumn(scalar, right) => {
                for (row, value) in right.word_values(index, selected)? {
                    let some = value.is_some();
                    let a = if some { *scalar } else { T::ZERO };
                    output.values[row].write(evaluate(a, value.unwrap_or(T::ONE))?);
                    present |= u64::from(some) << (row % 64);
                }
            }
            Self::Columns(left, right) => {
                let pairs = left
                    .word_values(index, selected)?
                    .zip(right.word_values(index, selected)?);
                for ((row, a), (_, b)) in pairs {
                    let some = a.is_some() && b.is_some();
                    let a = if some { a.unwrap_or(T::ZERO) } else { T::ZERO };
                    let b = if some { b.unwrap_or(T::ONE) } else { T::ONE };
                    output.values[row].write(evaluate(a, b)?);
                    present |= u64::from(some) << (row % 64);
                }
            }
        }
        output.non_nulls.set_word(index, present)
    }

    /// The whole-word operands of `index` with the non-NULL rows of the
    /// column operands, when every column operand exposes the word.
    #[cfg(all(target_arch = "aarch64", not(miri)))]
    fn blocks(&self, index: usize) -> Option<(Side<'_, T>, Side<'_, T>, u64)> {
        Some(match self {
            Self::ColumnScalar(left, scalar) => {
                let (lhs, present) = T::side(left.word_block(index)?);
                (lhs, Side::Scalar(*scalar), present)
            }
            Self::ScalarColumn(scalar, right) => {
                let (rhs, present) = T::side(right.word_block(index)?);
                (Side::Scalar(*scalar), rhs, present)
            }
            Self::Columns(left, right) => {
                let (lhs, left_present) = T::side(left.word_block(index)?);
                let (rhs, right_present) = T::side(right.word_block(index)?);
                (lhs, rhs, left_present & right_present)
            }
        })
    }

    #[cfg(not(all(target_arch = "aarch64", not(miri))))]
    fn blocks(&self, _index: usize) -> Option<(Side<'_, T>, Side<'_, T>, u64)> {
        None
    }
}

/// Check the dimensions, choose the operation and the strategy, and run.
fn run<T, L, R>(
    op: ArithOp,
    operands: Operands<'_, T, L, R>,
    rows: &RowMaskView<'_>,
    values: &mut [MaybeUninit<T>],
    non_nulls: &mut RowMask<'_>,
) -> Result<()>
where
    T: IntLane,
    L: ColumnReader<Value = T>,
    R: ColumnReader<Value = T>,
{
    let nrows = rows.nrows();
    ensure!(
        values.len() == nrows && non_nulls.as_view().nrows() == nrows,
        "result and selection row counts differ"
    );
    ensure!(
        operands
            .column_rows()
            .into_iter()
            .flatten()
            .all(|count| count == nrows),
        "column and selection row counts differ"
    );
    let output = Output {
        rows,
        values,
        non_nulls,
    };
    // The first word decides, as for the aggregates: selectivity is roughly
    // uniform within a batch, and a partly prepared batch refuses its first
    // word as it would the rest.
    let whole_words = cfg!(all(target_arch = "aarch64", not(miri)))
        && nrows >= 64
        && rows.word(0).is_some_and(|selected| {
            (selected == u64::MAX
                || (!selected.is_power_of_two() && selected.count_ones() >= BULK_MIN_ROWS))
                && operands.blocks(0).is_some()
        });
    // The operation is chosen once; each arm's function, the width's own,
    // is inlined into its own loops, unlike a trait object, which called per
    // row.
    match op {
        ArithOp::Add => execute(op, &operands, output, whole_words, &T::add),
        ArithOp::Sub => execute(op, &operands, output, whole_words, &T::sub),
        ArithOp::Mul => execute(op, &operands, output, whole_words, &T::mul),
        ArithOp::Div => execute(op, &operands, output, whole_words, &T::div),
        ArithOp::Mod => execute(op, &operands, output, whole_words, &T::rem),
    }
}

/// One operation on two non-NULL values of the width.
pub(crate) trait Evaluate<T>: Fn(T, T) -> Result<T, ArithmeticError> {}
impl<T, F: Fn(T, T) -> Result<T, ArithmeticError>> Evaluate<T> for F {}

fn execute<T, L, R, E: Evaluate<T>>(
    op: ArithOp,
    operands: &Operands<'_, T, L, R>,
    output: Output<'_, '_, '_, '_, T>,
    whole_words: bool,
    evaluate: &E,
) -> Result<()>
where
    T: IntLane,
    L: ColumnReader<Value = T>,
    R: ColumnReader<Value = T>,
{
    if whole_words {
        bulk(op, operands, output, evaluate)
    } else {
        operands.rows(output, evaluate)
    }
}

/// Whole words where every column operand exposes them, rows elsewhere; a
/// whole word is the width's own ([`IntLane::arith_block`]).
#[inline(never)]
fn bulk<T, L, R, E: Evaluate<T>>(
    op: ArithOp,
    operands: &Operands<'_, T, L, R>,
    output: Output<'_, '_, '_, '_, T>,
    evaluate: &E,
) -> Result<()>
where
    T: IntLane,
    L: ColumnReader<Value = T>,
    R: ColumnReader<Value = T>,
{
    let rows = output.rows;
    let mut output = output;
    // A scalar divisor of magnitude at least two divides by multiplication;
    // it is prepared once per call, before the loop: state changed inside
    // the loop reshaped it for every operation, and the compiler folds the
    // match of each whole word by the constant operation of each instance.
    let divisor = match (op, operands) {
        (ArithOp::Div | ArithOp::Mod, Operands::ColumnScalar(_, d)) => T::divisor(*d),
        _ => None,
    };
    for index in 0..rows.nrows().div_ceil(64) {
        let selected = rows.word_at(index);
        if selected == 0 || selected.is_power_of_two() {
            operands.word(index, selected, &mut output, evaluate)?;
            continue;
        }
        let Some((lhs, rhs, non_null)) = operands.blocks(index) else {
            operands.word(index, selected, &mut output, evaluate)?;
            continue;
        };
        let present = selected & non_null;
        let base = index * 64;
        let out: &mut [MaybeUninit<T>; 64] = (&mut output.values[base..base + 64])
            .try_into()
            .context("a whole-word operand implies a full word")?;
        T::arith_block(op, lhs, rhs, present, &divisor, out, evaluate)?;
        output.non_nulls.set_word(index, present)?;
    }
    Ok(())
}

/// The result buffers and the selection they follow.
struct Output<'r, 'v, 'm, 'w, T> {
    rows: &'r RowMaskView<'r>,
    values: &'v mut [MaybeUninit<T>],
    non_nulls: &'m mut RowMask<'w>,
}
