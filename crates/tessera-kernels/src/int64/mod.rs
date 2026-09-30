//! Signed int64 kernels without PostgreSQL type dispatch: comparisons
//! ([`filter`]), arithmetic ([`arith_scalar`], [`arith_scalar_left`],
//! [`arith_columns`]), aggregates ([`count`], [`min`], [`max`]; no sum,
//! which PostgreSQL computes in numeric) and key hashes ([`hash`],
//! [`hash_next`]), which agree with the int4 hashes on the int4 range.
//!
//! The family mirrors [`crate::int32`] kernel by kernel rather than sharing
//! a generic implementation over the lane type: the shape of every int32
//! loop was settled with the disassembler and the counters, and a
//! generalization would reshape both. What the families share is the
//! vocabulary of [`crate::ops`]. A physical int64 representation does not
//! select PostgreSQL semantics: the caller chooses kernels by logical type
//! and operation, and a Datum holds an int8 as its whole word.

mod aggregate;
mod arith;
mod compare;
mod divisor;
mod filter;
mod hash;

pub use aggregate::{max, min};
pub(crate) use arith::Side;
pub use arith::{arith_columns, arith_scalar, arith_scalar_left};
pub use compare::compare_columns;
pub(crate) use divisor::Divisor;
pub use filter::filter;
pub use hash::{NullKeys, fold, hash, hash_combine, hash_next, murmurhash32};

pub use crate::ops::{ArithOp, ArithmeticError, CompareOp};

pub(crate) use crate::BULK_MIN_ROWS;
