//! int4 arithmetic with PostgreSQL's rules, producing a dense column.
//!
//! `+`, `-` and `*` fail with [`ArithmeticError::IntegerOutOfRange`](crate::ops::ArithmeticError::IntegerOutOfRange) on
//! overflow; `/` and `%` fail with [`ArithmeticError::DivisionByZero`](crate::ops::ArithmeticError::DivisionByZero) on a
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
//!
//! When the first word of the selection is full, selects at least a dozen
//! rows and every column operand exposes its storage, the call computes
//! whole words: `+`, `-` and `*` with vector code on AArch64 (overflow
//! detected per lane and reported once per word); `/` and `%` by a scalar
//! divisor of magnitude at least two with a multiplier prepared once per
//! call (`Divisor`), applied to every lane by vector code and unable to
//! fail; other divisions lane by lane from the blocks, since NEON has no
//! integer division. Single-row words, the tail and refused words are read
//! row by row; every other call reads every word row by row through the
//! word iterators.

use std::mem::MaybeUninit;

use anyhow::Result;
use tessera_core::{ColumnReader, RowMask, RowMaskView};

use crate::ops::ArithOp;

/// Compute `column op scalar` for the selected rows into `values` and
/// `non_nulls`.
///
/// `values` and `non_nulls` have the batch's row count, and so must the
/// column. The operation is chosen once per call.
///
/// # Errors
///
/// Dimension errors fail before any mutation. A reader error (including an
/// unprepared selected row) and an [`ArithmeticError`](crate::ops::ArithmeticError) fail the call with
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
    crate::int::arith_scalar(op, column, scalar, rows, values, non_nulls)
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
    values: &mut [MaybeUninit<i32>],
    non_nulls: &mut RowMask<'_>,
) -> Result<()>
where
    L: ColumnReader<Value = i32>,
    R: ColumnReader<Value = i32>,
{
    crate::int::arith_columns(op, left, right, rows, values, non_nulls)
}

/// A whole-word operand of int4: the storage of a full prepared word, or a
/// constant.
pub(crate) type Side<'a> = crate::int::Side<'a, i32>;
