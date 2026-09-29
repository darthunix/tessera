//! Safe computational kernels over borrowed Tessera columns and row masks.
//!
//! Kernels do not depend on PostgreSQL or call it. Column storage stays owned
//! by the caller; kernels neither copy columns nor retain their borrows after
//! returning. Successful operations do not allocate; constructing an error may.
//! Mutating a row mask requires exclusive access. Concurrent calls must use
//! disjoint mutable masks and respect the chosen reader's sharing guarantees.
//!
//! [`int32::filter`] implements scalar comparisons, [`count::count`] the
//! non-NULL count of any type, [`int32::sum`], [`int32::min`] and
//! [`int32::max`] the aggregates and
//! [`int32::arith_scalar`] and its siblings the arithmetic with PostgreSQL's
//! error codes, and [`int32::hash`] and [`int32::hash_next`] the key hashes
//! of joins and grouping, all through [`tessera_core::ColumnReader`],
//! independently of physical storage; [`int64`] mirrors the family for
//! int8, kernel by kernel, with the vocabulary of [`ops`] shared, and
//! [`count::count`] counts non-NULL rows of any type and
//! [`cast::int4_to_int8`] widens int4 values into an int8 column.
//! [`decimal`] reads PostgreSQL's numeric of at most 18 digits as an `i64`
//! at its display scale from the stored bytes, computes with it exactly and
//! writes the numeric back, a batch at a time over a caller's source of
//! rows, leaving to the caller every row it does not take. [`calendar`]
//! is PostgreSQL's calendar of dates and timestamps: its Julian day
//! routines, the truncations, intervals and fields of the date functions.
//! Errors do not roll back previously completed words; callers must discard a
//! partial selection after failure. This crate does not introduce a C entry
//! point or catch panics. The future C boundary remains responsible for panic
//! handling.
//!
//! Full prepared words of representations that expose their storage
//! ([`tessera_core::WordBlock`]) are compared, aggregated, computed and
//! hashed with vector code on AArch64, division by a scalar included;
//! everything else takes the row paths. [`table`] is a hash table in a
//! region of memory the caller owns, for joins and grouping, addressed by
//! offsets so that the same bytes serve local and shared memory, and
//! [`sort`] orders a table's records by their integer keys. `unsafe`
//! is denied crate-wide and allowed only in two isolated modules: [`simd`],
//! for vector loads, and [`table`], for the region over raw pointers.

#![deny(unsafe_code)]

pub mod calendar;
pub mod cast;
pub mod count;
pub mod decimal;
pub mod int32;
pub mod int64;
pub mod ops;
mod simd;
pub mod sort;
pub mod spill_columns;
pub mod table;
