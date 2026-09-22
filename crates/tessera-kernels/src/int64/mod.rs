//! Signed int64 kernels without PostgreSQL type dispatch: comparisons
//! ([`filter`]); the arithmetic and the aggregates follow.
//!
//! The family mirrors [`crate::int32`] kernel by kernel rather than sharing
//! a generic implementation over the lane type: the shape of every int32
//! loop was settled with the disassembler and the counters, and a
//! generalization would reshape both. What the families share is the
//! vocabulary of [`crate::ops`]. A physical int64 representation does not
//! select PostgreSQL semantics: the caller chooses kernels by logical type
//! and operation, and a Datum holds an int8 as its whole word.

mod filter;

pub use filter::filter;

pub use crate::ops::CompareOp;
