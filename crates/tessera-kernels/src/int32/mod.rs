//! Signed int32 kernels without PostgreSQL type dispatch: comparisons
//! ([`filter`]), aggregates ([`count`], [`sum`], [`min`], [`max`]),
//! arithmetic ([`arith_scalar`], [`arith_scalar_left`], [`arith_columns`])
//! and key hashes ([`hash`], [`hash_next`]).
//!
//! A physical int32 representation does not select PostgreSQL semantics:
//! the future caller must choose kernels by logical type and operation.

mod aggregate;
mod arith;
pub(crate) mod divisor;
mod filter;
pub(crate) mod hash;

pub use crate::ops::{ArithOp, ArithmeticError, CompareOp};
pub use aggregate::{count, max, min, sum};
pub(crate) use arith::Side;
pub use arith::{arith_columns, arith_scalar, arith_scalar_left};
pub(crate) use divisor::Divisor;
pub use filter::filter;
pub use hash::{NullKeys, hash, hash_combine, hash_next, murmurhash32};

/// Selected rows in the first multi-row word from which whole-word kernels
/// pay for the call: on an M5 Pro a word costs 19 cycles dense and 27 cycles
/// Datum against about 2.5 cycles per selected row on the row path.
pub(crate) const BULK_MIN_ROWS: u32 = 12;
