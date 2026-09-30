//! int8 arithmetic with PostgreSQL's rules, producing a dense column.
//!
//! `+`, `-` and `*` fail with [`ArithmeticError::BigintOutOfRange`](crate::ops::ArithmeticError::BigintOutOfRange) on
//! overflow; `/` and `%` fail with [`ArithmeticError::DivisionByZero`](crate::ops::ArithmeticError::DivisionByZero) on a
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
//! the selection as its readiness mask.
//!
//! When the first word of the selection is full, selects at least a dozen
//! rows and every column operand exposes its storage, the call computes
//! whole words from the blocks: `+` and `-` with vector code on AArch64
//! (overflow detected per lane and reported once per word); `/` and `%` by
//! a scalar divisor of magnitude at least two through a multiplier
//! prepared once per call (`Divisor`), unable to fail; `*` and the other
//! divisions lane by lane over the present lanes, with their checks, since
//! NEON has no 64-bit multiply. Single-row words, the tail and refused
//! words are read row by row; every other call reads every word row by row
//! through the word iterators.

use std::mem::MaybeUninit;

use anyhow::Result;
use tessera_core::{ColumnReader, RowMask, RowMaskView};

use crate::ops::ArithOp;

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
    crate::int::arith_scalar(op, column, scalar, rows, values, non_nulls)
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
    crate::int::arith_scalar_left(op, scalar, column, rows, values, non_nulls)
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
    crate::int::arith_columns(op, left, right, rows, values, non_nulls)
}

/// A whole-word operand of int8: the storage of a full prepared word, or a
/// constant.
pub(crate) type Side<'a> = crate::int::Side<'a, i64>;
