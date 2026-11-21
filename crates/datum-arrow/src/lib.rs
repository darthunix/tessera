// The crate's page is its README.
#![doc = include_str!("../README.md")]
//! # Errors
//!
//! Every call that can refuse returns a [`Result`]. An [`Error`] is a
//! value of a few words: its [`ErrorKind`], with the SQLSTATE a
//! PostgreSQL server raises for it, the slot of the value that failed
//! where a value did, and a message for people.

// All memory comes to the crate as safe slices its caller makes, so the
// compiler alone keeps it sound.
#![forbid(unsafe_code)]
#![deny(missing_docs)]

/// Apache Arrow's types, the version the crate speaks, so that a caller
/// names the same [`DataType`](arrow_schema::DataType) the crate does.
pub use arrow_schema;

mod arrow;
mod bounds;
mod calendar;
mod column;
mod error;
mod failures;
mod pg;
#[cfg(kani)]
mod proofs;
mod read;
mod write;

/// A gigabyte of zero bytes that the tests of the limits of size share,
/// made once: they take slices of it and read none of its bytes, so its
/// pages are never touched.
#[cfg(test)]
fn zeros() -> &'static [u8] {
    static ZEROS: std::sync::LazyLock<Vec<u8>> = std::sync::LazyLock::new(|| vec![0u8; 1 << 30]);
    &ZEROS
}

pub use arrow::{ArrowType, Key, Plain, TimeUnit};
pub use bounds::{Comparison, RangeAnswer, Test};
pub use column::{Bitmap, Column, Input, Keys, Values, mask_words};
pub use error::{Error, ErrorKind, Result};
pub use failures::Failures;
pub use pg::{MAX_VARCHAR_LENGTH, PgType, VARHDRSZ, VarcharLength, oid};
pub use read::{MAX_VALUE_BYTES, ReadOptions, ReadOutput, ReadPlan, TextBytes, Unit};
pub use write::{MAX_WRITE_BYTES, WriteInput, WriteOutput, WritePlan, Written};
