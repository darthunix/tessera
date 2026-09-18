//! Safe computational kernels over borrowed Tessera columns and row masks.
//!
//! Kernels do not depend on PostgreSQL or call it. Column storage stays owned
//! by the caller; kernels neither copy columns nor retain their borrows after
//! returning. Successful operations do not allocate; constructing an error may.
//! Mutating a row mask requires exclusive access. Concurrent calls must use
//! disjoint mutable masks and respect the chosen reader's sharing guarantees.
//!
//! [`int32::filter`] implements scalar comparisons, [`int32::count`],
//! [`int32::sum`], [`int32::min`] and [`int32::max`] the aggregates and
//! [`int32::arith_scalar`] and its siblings the arithmetic with PostgreSQL's
//! error codes, and [`int32::hash`] and [`int32::hash_next`] the key hashes
//! of joins and grouping, all through [`tessera_core::ColumnReader`],
//! independently of physical storage.
//! Errors do not roll back previously completed words; callers must discard a
//! partial selection after failure. This crate does not introduce a C entry
//! point or catch panics. The future C boundary remains responsible for panic
//! handling.
//!
//! Full prepared words of representations that expose their storage
//! ([`tessera_core::WordBlock`]) are compared, aggregated and computed with
//! vector code on AArch64, division by a scalar included; everything else
//! takes the row paths. `unsafe` is denied crate-wide and
//! allowed only in the isolated [`simd`] module, for vector loads.

#![deny(unsafe_code)]

pub mod int32;
mod simd;
