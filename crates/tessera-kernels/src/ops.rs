//! The vocabulary every integer kernel family shares: the comparison and
//! arithmetic operations and the arithmetic failure with its SQLSTATE.
//!
//! The families ([`crate::int32`] and the ones that follow it) keep their
//! own kernels, since a generic implementation over the lane type would
//! reshape the vector code each family has verified; what they share is
//! the meaning of an operation and the error the C boundary reports, which
//! recognizes exactly one error type.

use std::fmt;

/// A comparison of a column value on the left with a non-NULL scalar on the right.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompareOp {
    /// Equal (`=`).
    Eq,
    /// Not equal (`!=`).
    Ne,
    /// Less than (`<`).
    Lt,
    /// Less than or equal (`<=`).
    Le,
    /// Greater than (`>`).
    Gt,
    /// Greater than or equal (`>=`).
    Ge,
}

/// A binary integer operation.
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
