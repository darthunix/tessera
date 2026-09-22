//! int8 arithmetic with PostgreSQL's rules, producing a dense column.
//!
//! `+`, `-` and `*` fail with [`ArithmeticError::BigintOutOfRange`] on
//! overflow; `/` and `%` fail with [`ArithmeticError::DivisionByZero`] on a
//! zero divisor, `/` also with `BigintOutOfRange` for `i64::MIN / -1`, and
//! `x % -1` is 0, as PostgreSQL defines it to avoid that trap. Division
//! truncates toward zero and the remainder takes the dividend's sign, as in
//! C, Rust and PostgreSQL. A NULL operand makes a NULL result, and the
//! checks apply to selected non-NULL rows only. An error anywhere in the
//! selection fails the whole call; the result is then unspecified and the
//! caller discards it. The errors carry their SQLSTATE for a C boundary
//! that reports them as PostgreSQL does, without PostgreSQL being called
//! from here.
//!
//! The result is a dense column in the caller's buffers: for every word with
//! selected rows the kernel writes the word of `non_nulls` (selected rows
//! whose operands are non-NULL) and the values of those rows; NULL rows get
//! an initialized placeholder, rows outside the selection are unspecified
//! and may stay uninitialized, and words without selected rows have their
//! `non_nulls` word cleared. The result reads as a `DenseInt64Column` with
//! the selection as its readiness mask. Every word is read row by row
//! through the word iterators; the whole-word path follows, measured
//! against this one.

use std::mem::MaybeUninit;

use anyhow::{Result, ensure};
use tessera_core::{ColumnReader, RowMask, RowMaskView};

use crate::ops::{ArithOp, ArithmeticError};

/// Compute `column op scalar` for the selected rows into `values` and
/// `non_nulls`.
///
/// `values` and `non_nulls` have the batch's row count, and so must the
/// column and the selection. Rows the selection excludes are unspecified
/// afterwards.
///
/// # Errors
///
/// Different row counts fail before any mutation. An arithmetic failure
/// or a reader error (an unprepared selected row) fails the call with the
/// outputs unspecified. Empty selections are valid.
pub fn arith_scalar<C: ColumnReader<Value = i64>>(
    op: ArithOp,
    column: &C,
    scalar: i64,
    rows: &RowMaskView<'_>,
    values: &mut [MaybeUninit<i64>],
    non_nulls: &mut RowMask<'_>,
) -> Result<()> {
    let operands: Operands<'_, C, C> = Operands::ColumnScalar(column, scalar);
    run(op, operands, rows, values, non_nulls)
}

/// Compute `scalar op column` for the selected rows into `values` and
/// `non_nulls`, for the operations where the order matters.
///
/// # Errors
///
/// As for [`arith_scalar`].
pub fn arith_scalar_left<C: ColumnReader<Value = i64>>(
    op: ArithOp,
    scalar: i64,
    column: &C,
    rows: &RowMaskView<'_>,
    values: &mut [MaybeUninit<i64>],
    non_nulls: &mut RowMask<'_>,
) -> Result<()> {
    let operands: Operands<'_, C, C> = Operands::ScalarColumn(scalar, column);
    run(op, operands, rows, values, non_nulls)
}

/// Compute `left op right` row by row for two columns of the batch; a NULL
/// on either side makes a NULL.
///
/// # Errors
///
/// As for [`arith_scalar`]; both columns must have the batch's row count.
pub fn arith_columns<L, R>(
    op: ArithOp,
    left: &L,
    right: &R,
    rows: &RowMaskView<'_>,
    values: &mut [MaybeUninit<i64>],
    non_nulls: &mut RowMask<'_>,
) -> Result<()>
where
    L: ColumnReader<Value = i64>,
    R: ColumnReader<Value = i64>,
{
    run(op, Operands::Columns(left, right), rows, values, non_nulls)
}

/// The operands of one call.
enum Operands<'a, L, R> {
    ColumnScalar(&'a L, i64),
    ScalarColumn(i64, &'a R),
    Columns(&'a L, &'a R),
}

impl<L, R> Operands<'_, L, R>
where
    L: ColumnReader<Value = i64>,
    R: ColumnReader<Value = i64>,
{
    fn column_rows(&self) -> [Option<usize>; 2] {
        match self {
            Self::ColumnScalar(left, _) => [Some(left.nrows()), None],
            Self::ScalarColumn(_, right) => [None, Some(right.nrows())],
            Self::Columns(left, right) => [Some(left.nrows()), Some(right.nrows())],
        }
    }

    /// Every word row by row. Out of line, as in the int32 kernel, so that
    /// the loop keeps its registers when a whole-word path shares the call.
    #[inline(never)]
    fn rows<E: Evaluate>(&self, output: Output<'_, '_, '_, '_>, evaluate: &E) -> Result<()> {
        let rows = output.rows;
        let mut output = output;
        for index in 0..rows.nrows().div_ceil(64) {
            let selected = rows.word(index).unwrap();
            self.word(index, selected, &mut output, evaluate)?;
        }
        Ok(())
    }

    /// One word row by row. The three operand shapes give three loops of
    /// one body; two columns are zipped word by word, which the trait
    /// guarantees to yield the same rows in the same order. A NULL row is
    /// computed from a placeholder pair that no operation rejects, so that
    /// the loop has no branch on nullness; only the error check branches,
    /// and it never goes. The pair is built from two scalars, not an
    /// `Option` of a tuple, which the compiler kept on the stack.
    #[inline(always)]
    fn word<E: Evaluate>(
        &self,
        index: usize,
        selected: u64,
        output: &mut Output<'_, '_, '_, '_>,
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
                    let b = if some { *scalar } else { 1 };
                    output.values[row].write(evaluate(value.unwrap_or(0), b)?);
                    present |= u64::from(some) << (row % 64);
                }
            }
            Self::ScalarColumn(scalar, right) => {
                for (row, value) in right.word_values(index, selected)? {
                    let some = value.is_some();
                    output.values[row].write(evaluate(*scalar, value.unwrap_or(1))?);
                    present |= u64::from(some) << (row % 64);
                }
            }
            Self::Columns(left, right) => {
                let pairs = left
                    .word_values(index, selected)?
                    .zip(right.word_values(index, selected)?);
                for ((row, a), (_, b)) in pairs {
                    let some = a.is_some() && b.is_some();
                    let b = if some { b.unwrap_or(1) } else { 1 };
                    output.values[row].write(evaluate(a.unwrap_or(0), b)?);
                    present |= u64::from(some) << (row % 64);
                }
            }
        }
        output.non_nulls.set_word(index, present)
    }
}

/// Check the dimensions, choose the operation, and run.
fn run<L, R>(
    op: ArithOp,
    operands: Operands<'_, L, R>,
    rows: &RowMaskView<'_>,
    values: &mut [MaybeUninit<i64>],
    non_nulls: &mut RowMask<'_>,
) -> Result<()>
where
    L: ColumnReader<Value = i64>,
    R: ColumnReader<Value = i64>,
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
    // The operation is chosen once; each arm's closure is inlined into its
    // own loops, unlike a trait object, which called per row.
    match op {
        ArithOp::Add => operands.rows(output, &|a: i64, b: i64| {
            a.checked_add(b).ok_or(ArithmeticError::BigintOutOfRange)
        }),
        ArithOp::Sub => operands.rows(output, &|a: i64, b: i64| {
            a.checked_sub(b).ok_or(ArithmeticError::BigintOutOfRange)
        }),
        ArithOp::Mul => operands.rows(output, &|a: i64, b: i64| {
            a.checked_mul(b).ok_or(ArithmeticError::BigintOutOfRange)
        }),
        ArithOp::Div => operands.rows(output, &|a: i64, b: i64| {
            if b == 0 {
                Err(ArithmeticError::DivisionByZero)
            } else {
                a.checked_div(b).ok_or(ArithmeticError::BigintOutOfRange)
            }
        }),
        ArithOp::Mod => operands.rows(output, &|a: i64, b: i64| {
            if b == 0 {
                Err(ArithmeticError::DivisionByZero)
            } else {
                Ok(a.wrapping_rem(b))
            }
        }),
    }
}

/// One int8 operation on two non-NULL values.
trait Evaluate: Fn(i64, i64) -> Result<i64, ArithmeticError> {}
impl<F: Fn(i64, i64) -> Result<i64, ArithmeticError>> Evaluate for F {}

/// The result buffers and the selection they follow.
struct Output<'r, 'v, 'm, 'w> {
    rows: &'r RowMaskView<'r>,
    values: &'v mut [MaybeUninit<i64>],
    non_nulls: &'m mut RowMask<'w>,
}
