//! The boundary between Tessera's Rust implementation and its C callers.
//!
//! This crate produces a static library but does not expose C functions yet.
//! On macOS and Linux, Cargo builds `target/debug/libtessera_capi.a` or
//! `target/release/libtessera_capi.a`. A Rust library is also built for Rust
//! consumers and tests. The C build does not yet link or install these libraries.
//!
//! # Borrowed column adapters
//!
//! [`DenseInt32Column`] and [`DatumInt32Column`] borrow possibly uninitialized
//! storage. Their unsafe constructors check dimensions; callers must guarantee
//! initialization and immutable storage as specified in each constructor's
//! safety contract. Subsequent [`tessera_core::ColumnReader`] operations check
//! readiness before exposing values. The adapters neither call PostgreSQL nor
//! own the backing buffers. Successful operations do not allocate; creating an
//! [`anyhow::Error`] may allocate.
//!
//! The Rust value type describes reading, not PostgreSQL operator semantics.
//! Future C dispatch must choose by PostgreSQL type, operation, collation, and
//! physical format. For example, an int32 buffer for `date` must not select an
//! int4 operator merely because the storage width matches.
//!
//! # Safety and errors at the C boundary
//!
//! Unsafe code is limited to this crate and future isolated SIMD modules.
//! Every unsafe block must have a `SAFETY` comment explaining its invariants.
//! The workspace enforces `unsafe_op_in_unsafe_fn = "deny"`.
//!
//! Rust must not call PostgreSQL, directly or through callbacks, or retain
//! `TupleTableSlot`, `MemoryContext`, or Datum pointers after returning to C.
//!
//! The following rules apply when the first C entry points are added:
//!
//! - Stack unwinding is allowed only within Rust. Both build profiles explicitly
//!   use `panic = "unwind"`; overriding this with `panic = "abort"` is unsupported.
//! - Entry points use `extern "C"`, not `extern "C-unwind"`. All potentially
//!   panicking work must run inside [`std::panic::catch_unwind`] within the Rust
//!   entry point. A caught panic becomes an error in the future `TessStatus`
//!   interface. `extern "C"` alone does not recover from a panic: an escaping
//!   panic aborts the process.
//! - Expected errors use `Result` and status returns, not panics. Catching a panic
//!   does not undo mutations: partial outputs must not be returned, and affected
//!   state must be restored or made unusable. Any [`std::panic::AssertUnwindSafe`]
//!   use must explain why state remains safe after unwinding.
//! - Resource cleanup, error handling, and dropping a caught panic's payload
//!   must not panic. Catching panics does not protect against aborts, memory
//!   corruption, or a second panic during unwinding.
//! - PostgreSQL's `ERROR` uses `siglongjmp`, not Rust unwinding, and is not caught
//!   by `catch_unwind`. C may call `ereport(ERROR)` only after Rust has returned;
//!   jumping over active Rust calls would bypass Rust resource cleanup.
//! - The library must not replace the global panic hook or call PostgreSQL from
//!   a hook. The hook runs before a panic is caught.
//!
//! The first real C entry point must include panic-to-status handling and a test
//! that calls it from C with an injected panic. The test must verify error return,
//! resource cleanup, no partial output, and safe state after failure with both
//! debug and release libraries. `cargo test` alone does not verify the library's
//! panic strategy because the test harness handles that setting separately.
//!
//! # Checking uninitialized-buffer access
//!
//! Use a compatible nightly containing Miri without changing the project's
//! pinned compiler. Ordinary tests also cover NULLs, unprepared rows, and
//! borrowing rules, but timing measurements do not establish memory safety.
//!
//! ```sh
//! cargo +nightly miri test -p tessera-capi --test columns --locked
//! cargo +nightly miri test -p tessera-core --test reader --locked
//! ```

mod column;

pub use column::{DatumInt32Column, DenseInt32Column};
