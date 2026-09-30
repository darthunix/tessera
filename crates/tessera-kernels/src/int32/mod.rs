//! Signed int32 kernels without PostgreSQL type dispatch: comparisons
//! ([`filter`]), aggregates ([`sum`], [`min`], [`max`]; the count is
//! [`crate::count::count`]'s), arithmetic ([`arith_scalar`],
//! [`arith_scalar_left`], [`arith_columns`]) and key hashes ([`hash`],
//! [`hash_next`]).
//!
//! A physical int32 representation does not select PostgreSQL semantics:
//! the future caller must choose kernels by logical type and operation.

mod aggregate;
mod arith;
mod compare;
pub(crate) mod divisor;
mod filter;
pub(crate) mod hash;

pub use crate::ops::{ArithOp, ArithmeticError, CompareOp};
pub use aggregate::{max, min, sum};
pub(crate) use arith::Side;
pub use arith::{arith_columns, arith_scalar, arith_scalar_left};
pub use compare::compare_columns;
pub(crate) use divisor::Divisor;
pub use filter::filter;
pub use hash::{NullKeys, hash, hash_combine, hash_next, murmurhash32};

pub(crate) use crate::BULK_MIN_ROWS;
