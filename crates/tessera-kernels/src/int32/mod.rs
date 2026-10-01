//! Signed int32 kernels without PostgreSQL type dispatch: comparisons
//! ([`filter`]), aggregates ([`sum`], [`min`], [`max`]; the count is
//! [`crate::count::count`]'s), arithmetic ([`arith_scalar`],
//! [`arith_scalar_left`], [`arith_columns`]) and key hashes ([`hash`],
//! [`hash_next`]).
//!
//! The drivers of the kernels are shared with [`crate::int64`] in
//! `crate::int`, generic over the lane type; the family's own is its lane
//! (`lane.rs`): how a Datum holds an int4, its operations and their error,
//! and the vector code of a whole word. A physical int32 representation
//! does not select PostgreSQL semantics: the caller chooses kernels by
//! logical type and operation.

mod aggregate;
mod arith;
mod compare;
pub(crate) mod divisor;
mod filter;
pub(crate) mod hash;
mod lane;

pub use crate::ops::{ArithOp, ArithmeticError, CompareOp};
pub use aggregate::{max, min, sum};
pub(crate) use arith::Side;
pub use arith::{arith_columns, arith_scalar, arith_scalar_left};
pub use compare::compare_columns;
pub(crate) use divisor::Divisor;
pub use filter::filter;
pub use hash::{NullKeys, hash, hash_combine, hash_next, murmurhash32};
