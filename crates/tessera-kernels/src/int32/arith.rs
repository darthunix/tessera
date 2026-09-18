//! int4 arithmetic with PostgreSQL's rules, producing a dense column.
//!
//! `+`, `-` and `*` fail with [`ArithmeticError::IntegerOutOfRange`] on
//! overflow; `/` and `%` fail with [`ArithmeticError::DivisionByZero`] on a
//! zero divisor, `/` also with `IntegerOutOfRange` for `i32::MIN / -1`, and
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
//! `non_nulls` word cleared. The result reads as a `DenseInt32Column` with
//! the selection as its readiness mask.

use std::fmt;
use std::mem::MaybeUninit;

use anyhow::{Result, ensure};
use tessera_core::{ColumnReader, RowMask, RowMaskView};

/// A binary int4 operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArithOp {
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `/`, truncating toward zero.
    Div,
    /// `%`, with the dividend's sign.
    Mod,
}

/// An arithmetic failure with the SQLSTATE PostgreSQL reports for it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArithmeticError {
    /// SQLSTATE 22003: an int4 result does not fit.
    IntegerOutOfRange,
    /// SQLSTATE 22012: a zero divisor.
    DivisionByZero,
}

impl ArithmeticError {
    /// The five-character SQLSTATE of the error.
    pub fn sqlstate(self) -> &'static str {
        match self {
            Self::IntegerOutOfRange => "22003",
            Self::DivisionByZero => "22012",
        }
    }
}

impl fmt::Display for ArithmeticError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::IntegerOutOfRange => "integer out of range",
            Self::DivisionByZero => "division by zero",
        })
    }
}

impl std::error::Error for ArithmeticError {}

/// Compute `column op scalar` for the selected rows into `values` and
/// `non_nulls`.
///
/// `values` and `non_nulls` have the batch's row count, and so must the
/// column. The operation is chosen once per call.
///
/// # Errors
///
/// Dimension errors fail before any mutation. A reader error (including an
/// unprepared selected row) and an [`ArithmeticError`] fail the call with
/// the result unspecified; the arithmetic error is recoverable from the
/// returned error with `downcast_ref::<ArithmeticError>()`.
///
/// ```
/// use std::mem::MaybeUninit;
/// use tessera_core::{ColumnView, RowMask, RowMaskView};
/// use tessera_kernels::int32::{ArithOp, arith_scalar};
///
/// let values = [10, 20, 30, 40];
/// let non_nulls = RowMaskView::try_new(4, &[0b1101])?;
/// let column = ColumnView::try_new(&values, Some(non_nulls))?;
/// let rows = RowMaskView::try_new(4, &[0b0111])?;
/// let mut out = [MaybeUninit::uninit(); 4];
/// let mut out_words = [0];
/// let mut out_non_nulls = RowMask::try_new(4, &mut out_words)?;
/// arith_scalar(ArithOp::Mul, &column, 3, &rows, &mut out, &mut out_non_nulls)?;
/// assert_eq!(out_words, [0b0101]);
/// // SAFETY: rows 0 and 2 are selected and non-NULL, so they were written.
/// assert_eq!(unsafe { (out[0].assume_init(), out[2].assume_init()) }, (30, 90));
/// # Ok::<(), anyhow::Error>(())
/// ```
pub fn arith_scalar<C: ColumnReader<Value = i32>>(
    op: ArithOp,
    column: &C,
    scalar: i32,
    rows: &RowMaskView<'_>,
    values: &mut [MaybeUninit<i32>],
    non_nulls: &mut RowMask<'_>,
) -> Result<()> {
    let output = Output::new(rows, values, non_nulls, &[column.nrows()])?;
    with_op(op, |evaluate| {
        output.evaluate(
            |index, selected| {
                Ok(column
                    .word_values(index, selected)?
                    .map(move |(row, value)| (row, value.map(|a| (a, scalar)))))
            },
            evaluate,
        )
    })
}

/// Compute `scalar op column`, for the operations where the order matters.
///
/// # Errors
///
/// As for [`arith_scalar`].
pub fn arith_scalar_left<C: ColumnReader<Value = i32>>(
    op: ArithOp,
    scalar: i32,
    column: &C,
    rows: &RowMaskView<'_>,
    values: &mut [MaybeUninit<i32>],
    non_nulls: &mut RowMask<'_>,
) -> Result<()> {
    let output = Output::new(rows, values, non_nulls, &[column.nrows()])?;
    with_op(op, |evaluate| {
        output.evaluate(
            |index, selected| {
                Ok(column
                    .word_values(index, selected)?
                    .map(move |(row, value)| (row, value.map(|b| (scalar, b)))))
            },
            evaluate,
        )
    })
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
    values: &mut [MaybeUninit<i32>],
    non_nulls: &mut RowMask<'_>,
) -> Result<()>
where
    L: ColumnReader<Value = i32>,
    R: ColumnReader<Value = i32>,
{
    let output = Output::new(rows, values, non_nulls, &[left.nrows(), right.nrows()])?;
    with_op(op, |evaluate| {
        output.evaluate(
            // The word iterators of two columns yield the same rows in the
            // same order for one selection word.
            |index, selected| {
                Ok(left
                    .word_values(index, selected)?
                    .zip(right.word_values(index, selected)?)
                    .map(|((row, a), (_, b))| (row, a.zip(b))))
            },
            evaluate,
        )
    })
}

/// Choose the operation once and run `body` with it.
fn with_op<T>(
    op: ArithOp,
    body: impl FnOnce(&dyn Fn(i32, i32) -> Result<i32, ArithmeticError>) -> T,
) -> T {
    match op {
        ArithOp::Add => body(&|a, b| a.checked_add(b).ok_or(ArithmeticError::IntegerOutOfRange)),
        ArithOp::Sub => body(&|a, b| a.checked_sub(b).ok_or(ArithmeticError::IntegerOutOfRange)),
        ArithOp::Mul => body(&|a, b| a.checked_mul(b).ok_or(ArithmeticError::IntegerOutOfRange)),
        ArithOp::Div => body(&|a, b| {
            if b == 0 {
                Err(ArithmeticError::DivisionByZero)
            } else {
                a.checked_div(b).ok_or(ArithmeticError::IntegerOutOfRange)
            }
        }),
        ArithOp::Mod => body(&|a, b| {
            if b == 0 {
                Err(ArithmeticError::DivisionByZero)
            } else {
                Ok(a.wrapping_rem(b))
            }
        }),
    }
}

/// The result buffers and the selection they follow.
struct Output<'r, 'v, 'm, 'w> {
    rows: &'r RowMaskView<'r>,
    values: &'v mut [MaybeUninit<i32>],
    non_nulls: &'m mut RowMask<'w>,
}

impl<'r, 'v, 'm, 'w> Output<'r, 'v, 'm, 'w> {
    /// Check every dimension before anything is written.
    fn new(
        rows: &'r RowMaskView<'r>,
        values: &'v mut [MaybeUninit<i32>],
        non_nulls: &'m mut RowMask<'w>,
        column_rows: &[usize],
    ) -> Result<Self> {
        let nrows = rows.nrows();
        ensure!(
            values.len() == nrows && non_nulls.as_view().nrows() == nrows,
            "result and selection row counts differ"
        );
        ensure!(
            column_rows.iter().all(|&count| count == nrows),
            "column and selection row counts differ"
        );
        Ok(Self {
            rows,
            values,
            non_nulls,
        })
    }

    /// Every selected row: a NULL row gets a placeholder computed from
    /// `(0, 1)`, which no operation rejects, so that the loop has no branch
    /// on nullness; only the error check branches, and it never goes.
    fn evaluate<I>(
        self,
        mut pairs: impl FnMut(usize, u64) -> Result<I>,
        evaluate: &dyn Fn(i32, i32) -> Result<i32, ArithmeticError>,
    ) -> Result<()>
    where
        I: Iterator<Item = (usize, Option<(i32, i32)>)>,
    {
        let Self {
            rows,
            values,
            non_nulls,
        } = self;
        for index in 0..rows.nrows().div_ceil(64) {
            let selected = rows.word(index).unwrap();
            if selected == 0 {
                non_nulls.set_word(index, 0)?;
                continue;
            }
            let mut present = 0;
            for (row, pair) in pairs(index, selected)? {
                let (a, b) = pair.unwrap_or((0, 1));
                values[row].write(evaluate(a, b)?);
                present |= u64::from(pair.is_some()) << (row % 64);
            }
            non_nulls.set_word(index, present)?;
        }
        Ok(())
    }
}
