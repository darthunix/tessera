//! Signed int32 kernels without PostgreSQL type dispatch: comparisons
//! ([`filter`]), aggregates ([`count`], [`sum`], [`min`], [`max`]) and
//! arithmetic ([`arith_scalar`], [`arith_scalar_left`], [`arith_columns`]).
//!
//! A physical int32 representation does not select PostgreSQL semantics:
//! the future caller must choose kernels by logical type and operation.

mod aggregate;
mod arith;
mod filter;

pub use aggregate::{count, max, min, sum};
pub use arith::{ArithOp, ArithmeticError, arith_columns, arith_scalar, arith_scalar_left};
pub use filter::filter;

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

/// Selected rows in the first multi-row word from which whole-word kernels
/// pay for the call: on an M5 Pro a word costs 19 cycles dense and 27 cycles
/// Datum against about 2.5 cycles per selected row on the row path.
pub(crate) const BULK_MIN_ROWS: u32 = 12;
