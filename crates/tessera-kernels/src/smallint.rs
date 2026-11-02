//! smallint arithmetic with a smallint result: `+`, `-`, `*`, `/` and `%`
//! of two smallints, and with them `-x`, which is `0 - x`, and the cast of
//! an int4 to smallint, which is `x + 0`.
//!
//! A smallint lives in an int4 lane, its value sign-extended, and no
//! operation of two smallints overflows an int4: these are the int4 kernels
//! of [`crate::int32`], whose result must also fall within -32768 to
//! 32767. A result past it fails with
//! [`ArithmeticError::SmallintOutOfRange`] (22003 "smallint out of
//! range"), as PostgreSQL's smallint operators do. The check is part of
//! each row's step and of each whole word's vector code, so the call fails
//! with the error of the first row that fails in row order, a range error
//! or a division by zero, as the core meets them row by row. Otherwise the
//! kernels are [`crate::int32`]'s: the same shapes, NULLs, buffers and
//! errors of a misuse. The operands must be smallints, but for one use:
//! `x + 0`, the cast of an int4 to smallint, whose `x` may be any int4.
//! The int4 sum of it cannot overflow, and the range check refuses a value
//! past the smallint range. Any other operand outside the range is a
//! misuse, whose result is unspecified.

use std::mem::MaybeUninit;

use anyhow::Result;
use tessera_core::{ColumnReader, RowMask, RowMaskView};

use crate::int::Smallint;
pub use crate::ops::{ArithOp, ArithmeticError};

/// `column op scalar` for the selected rows into `values` and `non_nulls`,
/// a smallint result.
///
/// # Errors
///
/// As [`crate::int32::arith_scalar`], and
/// [`ArithmeticError::SmallintOutOfRange`] for a result past the range.
///
/// ```
/// use std::mem::MaybeUninit;
/// use tessera_core::{ColumnView, RowMask, RowMaskView};
/// use tessera_kernels::smallint::{ArithOp, ArithmeticError, arith_scalar};
///
/// let values = [32000, -5, 7];
/// let column = ColumnView::try_new(&values, None)?;
/// let rows = RowMaskView::try_new(3, &[0b110])?;
/// let mut out = [MaybeUninit::uninit(); 3];
/// let mut out_words = [0];
/// let mut out_non_nulls = RowMask::try_new(3, &mut out_words)?;
/// arith_scalar(ArithOp::Mul, &column, 1000, &rows, &mut out, &mut out_non_nulls)?;
/// // SAFETY: rows 1 and 2 are selected and non-NULL, so they were written.
/// assert_eq!(unsafe { (out[1].assume_init(), out[2].assume_init()) }, (-5000, 7000));
/// let rows = RowMaskView::try_new(3, &[0b111])?;
/// let error = arith_scalar(ArithOp::Mul, &column, 1000, &rows, &mut out, &mut out_non_nulls)
///     .unwrap_err();
/// assert_eq!(
///     error.downcast_ref::<ArithmeticError>(),
///     Some(&ArithmeticError::SmallintOutOfRange)
/// );
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
    crate::int::arith_scalar::<_, Smallint, _>(op, column, scalar, rows, values, non_nulls)
}

/// `scalar op column`, a smallint result.
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
    crate::int::arith_scalar_left::<_, Smallint, _>(op, scalar, column, rows, values, non_nulls)
}

/// `left op right` for two columns of the batch, a smallint result.
///
/// # Errors
///
/// As for [`arith_scalar`].
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
    crate::int::arith_columns::<_, Smallint, _, _>(op, left, right, rows, values, non_nulls)
}
