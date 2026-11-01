//! PostgreSQL's float functions over float4 (`f32`) and float8 (`f64`):
//! the comparisons, the arithmetic with its checks, and the casts between
//! floats and integers, as the core computes them in `utils/float.h` and
//! `utils/adt/float.c`.
//!
//! - **Order.** NaN equals NaN and is greater than every other value; -0
//!   equals 0. A float4 compared with a float8 is widened, which is exact.
//! - **Arithmetic.** An infinite result of finite operands is an overflow.
//!   A zero product of two non-zero operands, or a zero quotient of a
//!   non-zero dividend by a finite divisor, is an underflow. A zero divisor
//!   of a dividend that is not NaN is a division by zero, checked first.
//!   Operands of both widths are computed in float8.
//! - **Casts.** A float becomes an integer rounded half to even, as `rint`
//!   rounds, then checked against the range in its own width: at least the
//!   type's least value and below its negation. A float8 narrowed to float4
//!   overflows or underflows as above. An integer becomes a float rounded
//!   to nearest.
//!
//! Each error is a [`FloatError`] with the core's SQLSTATE and message. The
//! batch functions run over the selected rows of a [`Source`] in the order
//! of the rows, so a call fails at the row the core would raise at. A float
//! result is written as its bits, a float4's sign-extended to 64, as
//! PostgreSQL's Datum holds it.

use std::fmt;
use std::mem::MaybeUninit;
use std::ops::{Add, Div, Mul, Neg, Sub};

use anyhow::Result;
use tessera_core::{RowMask, RowMaskView, ones};

use crate::ops::CompareOp;
use crate::source::{Source, Unary, map_rows};

/// An error of a float function, as the core raises it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FloatError {
    /// An infinite result of finite operands (22003).
    Overflow,
    /// A zero result of non-zero operands (22003).
    Underflow,
    /// A zero divisor (22012).
    DivisionByZero,
    /// A cast to int4 out of its range (22003).
    IntegerOutOfRange,
    /// A cast to int2 out of its range (22003).
    SmallintOutOfRange,
    /// A cast to int8 out of its range (22003).
    BigintOutOfRange,
}

impl FloatError {
    /// The five-character SQLSTATE the core raises.
    pub fn sqlstate(self) -> &'static str {
        match self {
            Self::DivisionByZero => "22012",
            _ => "22003",
        }
    }
}

impl fmt::Display for FloatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Overflow => "value out of range: overflow",
            Self::Underflow => "value out of range: underflow",
            Self::DivisionByZero => "division by zero",
            Self::IntegerOutOfRange => "integer out of range",
            Self::SmallintOutOfRange => "smallint out of range",
            Self::BigintOutOfRange => "bigint out of range",
        })
    }
}

impl std::error::Error for FloatError {}

/// `left op right` in the core's order of floats.
#[inline(always)]
pub fn compare(op: CompareOp, left: f64, right: f64) -> bool {
    match op {
        CompareOp::Eq => {
            if left.is_nan() {
                right.is_nan()
            } else {
                !right.is_nan() && left == right
            }
        }
        CompareOp::Ne => {
            if left.is_nan() {
                !right.is_nan()
            } else {
                right.is_nan() || left != right
            }
        }
        CompareOp::Lt => !left.is_nan() && (right.is_nan() || left < right),
        CompareOp::Le => right.is_nan() || (!left.is_nan() && left <= right),
        CompareOp::Gt => !right.is_nan() && (left.is_nan() || left > right),
        CompareOp::Ge => left.is_nan() || (!right.is_nan() && left >= right),
    }
}

/// An arithmetic operation of floats.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FloatOp {
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `/`
    Div,
    /// Unary `-`.
    Neg,
    /// `abs` (`@`).
    Abs,
}

impl FloatOp {
    /// Whether the operation takes one argument.
    pub fn is_unary(self) -> bool {
        matches!(self, Self::Neg | Self::Abs)
    }
}

/// A float of either width: what the checked arithmetic needs of it.
pub trait Float:
    Copy
    + PartialEq
    + Add<Output = Self>
    + Sub<Output = Self>
    + Mul<Output = Self>
    + Div<Output = Self>
    + Neg<Output = Self>
{
    /// Zero.
    const ZERO: Self;
    /// Whether the value is NaN.
    fn is_nan(self) -> bool;
    /// Whether the value is an infinity.
    fn is_infinite(self) -> bool;
    /// The value with its sign bit cleared, as `fabs` gives it.
    fn abs(self) -> Self;
}

impl Float for f32 {
    const ZERO: Self = 0.0;

    #[inline(always)]
    fn is_nan(self) -> bool {
        f32::is_nan(self)
    }

    #[inline(always)]
    fn is_infinite(self) -> bool {
        f32::is_infinite(self)
    }

    #[inline(always)]
    fn abs(self) -> Self {
        f32::abs(self)
    }
}

impl Float for f64 {
    const ZERO: Self = 0.0;

    #[inline(always)]
    fn is_nan(self) -> bool {
        f64::is_nan(self)
    }

    #[inline(always)]
    fn is_infinite(self) -> bool {
        f64::is_infinite(self)
    }

    #[inline(always)]
    fn abs(self) -> Self {
        f64::abs(self)
    }
}

/// A sum or a difference, as `float8_pl` and `float8_mi` check it.
#[inline(always)]
fn summed<F: Float>(result: F, left: F, right: F) -> Result<F, FloatError> {
    if result.is_infinite() && !left.is_infinite() && !right.is_infinite() {
        return Err(FloatError::Overflow);
    }
    Ok(result)
}

/// `left op right`, or `op left` for a unary operation (`right` unread),
/// as the core's function of the operation computes and checks it.
#[inline(always)]
pub fn arith<F: Float>(op: FloatOp, left: F, right: F) -> Result<F, FloatError> {
    match op {
        FloatOp::Add => summed(left + right, left, right),
        FloatOp::Sub => summed(left - right, left, right),
        FloatOp::Mul => {
            let result = left * right;
            if result.is_infinite() && !left.is_infinite() && !right.is_infinite() {
                return Err(FloatError::Overflow);
            }
            if result == F::ZERO && left != F::ZERO && right != F::ZERO {
                return Err(FloatError::Underflow);
            }
            Ok(result)
        }
        FloatOp::Div => {
            if right == F::ZERO && !left.is_nan() {
                return Err(FloatError::DivisionByZero);
            }
            let result = left / right;
            if result.is_infinite() && !left.is_infinite() {
                return Err(FloatError::Overflow);
            }
            if result == F::ZERO && left != F::ZERO && !right.is_infinite() {
                return Err(FloatError::Underflow);
            }
            Ok(result)
        }
        FloatOp::Neg => Ok(-left),
        FloatOp::Abs => Ok(left.abs()),
    }
}

/// A value a cast to a float reads: an int4 (an int2 too), an int8 or a
/// float.
pub trait Number: Copy {
    /// The value as a float8, rounded to nearest.
    fn float8(self) -> f64;
    /// The value as a float4, rounded to nearest; a float8 checked as
    /// `dtof` checks it.
    fn float4(self) -> Result<f32, FloatError>;
}

impl Number for i32 {
    #[inline(always)]
    fn float8(self) -> f64 {
        f64::from(self)
    }

    #[inline(always)]
    fn float4(self) -> Result<f32, FloatError> {
        Ok(self as f32)
    }
}

impl Number for i64 {
    #[inline(always)]
    fn float8(self) -> f64 {
        self as f64
    }

    #[inline(always)]
    fn float4(self) -> Result<f32, FloatError> {
        Ok(self as f32)
    }
}

impl Number for f32 {
    #[inline(always)]
    fn float8(self) -> f64 {
        f64::from(self)
    }

    #[inline(always)]
    fn float4(self) -> Result<f32, FloatError> {
        Ok(self)
    }
}

impl Number for f64 {
    #[inline(always)]
    fn float8(self) -> f64 {
        self
    }

    #[inline(always)]
    fn float4(self) -> Result<f32, FloatError> {
        let narrow = self as f32;
        if narrow.is_infinite() && !self.is_infinite() {
            return Err(FloatError::Overflow);
        }
        if narrow == 0.0 && self != 0.0 {
            return Err(FloatError::Underflow);
        }
        Ok(narrow)
    }
}

/// The integer type a float is cast to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Integer {
    /// int2 (smallint).
    Int2,
    /// int4 (integer).
    Int4,
    /// int8 (bigint).
    Int8,
}

impl Integer {
    /// The type's least value, whose negation bounds it from above.
    fn least(self) -> i64 {
        match self {
            Self::Int2 => i64::from(i16::MIN),
            Self::Int4 => i64::from(i32::MIN),
            Self::Int8 => i64::MIN,
        }
    }

    /// The error of a value out of the type's range.
    fn out_of_range(self) -> FloatError {
        match self {
            Self::Int2 => FloatError::SmallintOutOfRange,
            Self::Int4 => FloatError::IntegerOutOfRange,
            Self::Int8 => FloatError::BigintOutOfRange,
        }
    }
}

/// A float cast to an integer: rounded half to even, then checked in its
/// own width, as `dtoi4`, `ftoi4` and their siblings check it.
pub trait Rounded: Copy {
    /// The value rounded and checked against `to`'s range.
    fn integer(self, to: Integer) -> Result<i64, FloatError>;
}

impl Rounded for f32 {
    #[inline(always)]
    fn integer(self, to: Integer) -> Result<i64, FloatError> {
        let rounded = self.round_ties_even();
        // The least value of each type is a power of two, exact in float4.
        let least = to.least() as f32;
        if rounded >= least && rounded < -least {
            Ok(rounded as i64)
        } else {
            Err(to.out_of_range())
        }
    }
}

impl Rounded for f64 {
    #[inline(always)]
    fn integer(self, to: Integer) -> Result<i64, FloatError> {
        let rounded = self.round_ties_even();
        let least = to.least() as f64;
        if rounded >= least && rounded < -least {
            Ok(rounded as i64)
        } else {
            Err(to.out_of_range())
        }
    }
}

/// `left op right` over the selected rows: keeps in `rows` the rows where
/// both are not NULL and the comparison holds, in the core's order.
///
/// # Errors
///
/// None of the data; a source that panics on a row past it.
pub fn compare_rows<L, R>(
    op: CompareOp,
    left: &impl Source<L>,
    right: &impl Source<R>,
    rows: &mut RowMask<'_>,
) -> Result<()>
where
    L: Copy + Into<f64>,
    R: Copy + Into<f64>,
{
    match op {
        CompareOp::Eq => keep_rows(left, right, rows, |l, r| compare(CompareOp::Eq, l, r)),
        CompareOp::Ne => keep_rows(left, right, rows, |l, r| compare(CompareOp::Ne, l, r)),
        CompareOp::Lt => keep_rows(left, right, rows, |l, r| compare(CompareOp::Lt, l, r)),
        CompareOp::Le => keep_rows(left, right, rows, |l, r| compare(CompareOp::Le, l, r)),
        CompareOp::Gt => keep_rows(left, right, rows, |l, r| compare(CompareOp::Gt, l, r)),
        CompareOp::Ge => keep_rows(left, right, rows, |l, r| compare(CompareOp::Ge, l, r)),
    }
}

/// Keep in `rows` the selected rows whose arguments are not NULL and
/// satisfy `keep`, one loop for each `keep`.
#[inline(always)]
fn keep_rows<L, R>(
    left: &impl Source<L>,
    right: &impl Source<R>,
    rows: &mut RowMask<'_>,
    keep: impl Fn(f64, f64) -> bool,
) -> Result<()>
where
    L: Copy + Into<f64>,
    R: Copy + Into<f64>,
{
    for word in 0..rows.as_view().nrows().div_ceil(64) {
        let look = rows.as_view().word(word);
        let mut kept = 0;
        for bit in ones(look) {
            let row = word * 64 + bit;
            let (Some(left), Some(right)) = (left.get(row), right.get(row)) else {
                continue;
            };
            if keep(left.into(), right.into()) {
                kept |= 1 << bit;
            }
        }
        rows.set_word(word, kept)?;
    }
    Ok(())
}

/// The arithmetic of two float4 over the selected rows, a float4 result
/// written as its bits.
///
/// # Errors
///
/// The first error of a row ([`FloatError`]), or masks and arrays of
/// different row counts. A unary operation is refused: see
/// [`unary_float4`].
pub fn binary_float4(
    op: FloatOp,
    left: &impl Source<f32>,
    right: &impl Source<f32>,
    rows: RowMaskView<'_>,
    values: &mut [MaybeUninit<u64>],
    non_nulls: &mut RowMask<'_>,
) -> Result<()> {
    match op {
        FloatOp::Add => map_rows(left, right, rows, values, non_nulls, |l, r| {
            arith(FloatOp::Add, l, r).map(float4_bits)
        }),
        FloatOp::Sub => map_rows(left, right, rows, values, non_nulls, |l, r| {
            arith(FloatOp::Sub, l, r).map(float4_bits)
        }),
        FloatOp::Mul => map_rows(left, right, rows, values, non_nulls, |l, r| {
            arith(FloatOp::Mul, l, r).map(float4_bits)
        }),
        FloatOp::Div => map_rows(left, right, rows, values, non_nulls, |l, r| {
            arith(FloatOp::Div, l, r).map(float4_bits)
        }),
        FloatOp::Neg | FloatOp::Abs => anyhow::bail!("{op:?} takes one argument"),
    }
}

/// The arithmetic of two floats, either float8 or one of them float4
/// widened, over the selected rows, a float8 result written as its bits.
///
/// # Errors
///
/// As [`binary_float4`].
pub fn binary_float8<L, R>(
    op: FloatOp,
    left: &impl Source<L>,
    right: &impl Source<R>,
    rows: RowMaskView<'_>,
    values: &mut [MaybeUninit<u64>],
    non_nulls: &mut RowMask<'_>,
) -> Result<()>
where
    L: Copy + Into<f64>,
    R: Copy + Into<f64>,
{
    match op {
        FloatOp::Add => map_rows(left, right, rows, values, non_nulls, |l, r| {
            arith(FloatOp::Add, l.into(), r.into()).map(f64::to_bits)
        }),
        FloatOp::Sub => map_rows(left, right, rows, values, non_nulls, |l, r| {
            arith(FloatOp::Sub, l.into(), r.into()).map(f64::to_bits)
        }),
        FloatOp::Mul => map_rows(left, right, rows, values, non_nulls, |l, r| {
            arith(FloatOp::Mul, l.into(), r.into()).map(f64::to_bits)
        }),
        FloatOp::Div => map_rows(left, right, rows, values, non_nulls, |l, r| {
            arith(FloatOp::Div, l.into(), r.into()).map(f64::to_bits)
        }),
        FloatOp::Neg | FloatOp::Abs => anyhow::bail!("{op:?} takes one argument"),
    }
}

/// `-x` or `abs(x)` of a float4 over the selected rows.
///
/// # Errors
///
/// A binary operation, or masks and arrays of different row counts.
pub fn unary_float4(
    op: FloatOp,
    source: &impl Source<f32>,
    rows: RowMaskView<'_>,
    values: &mut [MaybeUninit<u64>],
    non_nulls: &mut RowMask<'_>,
) -> Result<()> {
    match op {
        FloatOp::Neg => map_rows(source, &Unary, rows, values, non_nulls, |x: f32, ()| {
            Ok::<_, FloatError>(float4_bits(-x))
        }),
        FloatOp::Abs => map_rows(source, &Unary, rows, values, non_nulls, |x: f32, ()| {
            Ok::<_, FloatError>(float4_bits(x.abs()))
        }),
        _ => anyhow::bail!("{op:?} takes two arguments"),
    }
}

/// `-x` or `abs(x)` of a float8 over the selected rows.
///
/// # Errors
///
/// As [`unary_float4`].
pub fn unary_float8(
    op: FloatOp,
    source: &impl Source<f64>,
    rows: RowMaskView<'_>,
    values: &mut [MaybeUninit<u64>],
    non_nulls: &mut RowMask<'_>,
) -> Result<()> {
    match op {
        FloatOp::Neg => map_rows(source, &Unary, rows, values, non_nulls, |x: f64, ()| {
            Ok::<_, FloatError>((-x).to_bits())
        }),
        FloatOp::Abs => map_rows(source, &Unary, rows, values, non_nulls, |x: f64, ()| {
            Ok::<_, FloatError>(x.abs().to_bits())
        }),
        _ => anyhow::bail!("{op:?} takes two arguments"),
    }
}

/// The selected rows cast to float8, written as its bits.
///
/// # Errors
///
/// Masks and arrays of different row counts.
pub fn to_float8<T: Number>(
    source: &impl Source<T>,
    rows: RowMaskView<'_>,
    values: &mut [MaybeUninit<u64>],
    non_nulls: &mut RowMask<'_>,
) -> Result<()> {
    map_rows(source, &Unary, rows, values, non_nulls, |x: T, ()| {
        Ok::<_, FloatError>(x.float8().to_bits())
    })
}

/// The selected rows cast to float4, written as its bits sign-extended,
/// as a Datum holds them.
///
/// # Errors
///
/// The first float8 that overflows or underflows float4, or masks and
/// arrays of different row counts.
pub fn to_float4<T: Number>(
    source: &impl Source<T>,
    rows: RowMaskView<'_>,
    values: &mut [MaybeUninit<u64>],
    non_nulls: &mut RowMask<'_>,
) -> Result<()> {
    map_rows(source, &Unary, rows, values, non_nulls, |x: T, ()| {
        x.float4().map(float4_bits)
    })
}

/// The selected rows cast to int4 (`to` [`Integer::Int4`]) or int2
/// ([`Integer::Int2`]), written as int32.
///
/// # Errors
///
/// The first value out of `to`'s range, an int8 `to`, or masks and arrays
/// of different row counts.
pub fn to_int32<T: Rounded>(
    to: Integer,
    source: &impl Source<T>,
    rows: RowMaskView<'_>,
    values: &mut [MaybeUninit<i32>],
    non_nulls: &mut RowMask<'_>,
) -> Result<()> {
    match to {
        // The range checked, the value fits an int32.
        Integer::Int4 => map_rows(source, &Unary, rows, values, non_nulls, |x: T, ()| {
            x.integer(Integer::Int4).map(|value| value as i32)
        }),
        Integer::Int2 => map_rows(source, &Unary, rows, values, non_nulls, |x: T, ()| {
            x.integer(Integer::Int2).map(|value| value as i32)
        }),
        Integer::Int8 => anyhow::bail!("an int8 result takes 64-bit values"),
    }
}

/// The selected rows cast to int8.
///
/// # Errors
///
/// The first value out of int8's range, or masks and arrays of different
/// row counts.
pub fn to_int64<T: Rounded>(
    source: &impl Source<T>,
    rows: RowMaskView<'_>,
    values: &mut [MaybeUninit<i64>],
    non_nulls: &mut RowMask<'_>,
) -> Result<()> {
    map_rows(source, &Unary, rows, values, non_nulls, |x: T, ()| {
        x.integer(Integer::Int8)
    })
}

/// A float4's bits as a Datum holds them: `Float4GetDatum` makes them an
/// int32 and `Int32GetDatum` extends its sign.
#[inline(always)]
fn float4_bits(value: f32) -> u64 {
    i64::from(value.to_bits() as i32) as u64
}

#[cfg(test)]
mod tests;

#[cfg(kani)]
mod proofs;
