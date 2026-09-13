//! Safe computational kernels over borrowed Tessera columns and row masks.
//!
//! Kernels do not depend on PostgreSQL or call it. Column storage stays owned
//! by the caller; kernels neither copy columns nor retain their borrows after
//! returning. Successful operations do not allocate; constructing an error may.
//! Mutating a row mask requires exclusive access. Concurrent calls must use
//! disjoint mutable masks and respect the chosen reader's sharing guarantees.
//!
//! [`int32::filter`] implements scalar comparisons through
//! [`tessera_core::ColumnReader`], independently of physical storage. Errors do
//! not roll back previously completed words; callers must discard a partial
//! selection after failure. This crate does not introduce a C entry point or
//! catch panics. The future C boundary remains responsible for panic handling.

#![forbid(unsafe_code)]

pub mod int32;
