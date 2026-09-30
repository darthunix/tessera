//! Signed int64 kernels without PostgreSQL type dispatch: comparisons
//! ([`filter`], [`compare_columns`]), arithmetic ([`arith_scalar`],
//! [`arith_scalar_left`], [`arith_columns`]), aggregates ([`min`], [`max`];
//! the count is [`crate::count::count`]'s, and there is no sum, which
//! PostgreSQL computes in numeric) and key hashes ([`hash`],
//! [`hash_next`]), which agree with the int4 hashes on the int4 range.
//!
//! The family shares with [`crate::int32`] the vocabulary of [`crate::ops`]
//! and every driver, in `crate::int`, generic over the lane type: the
//! choice between whole words and rows, the row loops and the loops over
//! whole words. What stays the family's own is its lane (`lane.rs`): how a
//! Datum holds an int8, its operations and their error, and the vector code
//! of a whole word, whose lanes and gaps differ (NEON has no 64-bit
//! multiplication, so `*` and a prepared division go lane by lane). A
//! physical int64 representation does not select PostgreSQL semantics: the
//! caller chooses kernels by logical type and operation, and a Datum holds
//! an int8 as its whole word.

mod aggregate;
mod arith;
mod compare;
mod divisor;
mod filter;
mod hash;
mod lane;

pub use aggregate::{max, min};
pub(crate) use arith::Side;
pub use arith::{arith_columns, arith_scalar, arith_scalar_left};
pub use compare::compare_columns;
pub(crate) use divisor::Divisor;
pub use filter::filter;
pub use hash::{NullKeys, fold, hash, hash_combine, hash_next, murmurhash32};

pub use crate::ops::{ArithOp, ArithmeticError, CompareOp};
