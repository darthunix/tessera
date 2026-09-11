# Tessera

A tessera is one of the small tiles used to make an ancient mosaic. This
project follows the same idea: small, independently tested modules combine to
form a complete batch execution engine for PostgreSQL.

Tessera will provide a bridge between extensions, a batch runtime,
computational kernels, reusable spill support, executor nodes, and example
data sources. Together, these modules are intended to make batch executors
composable in the same spirit as DataFusion.

Tessera currently targets PostgreSQL master and is not ready for production
use.

See the [bridge guide](docs/bridge.md) for the public C API, ownership rules,
and a runnable example using independent producer and consumer modules.

## Rust workspace

The Rust workspace currently contains two crate skeletons:

- `tessera-core`: safe data structures and algorithms, with unsafe code
  forbidden and no PostgreSQL dependency.
- `tessera-capi`: a static library for the future C interface, depending on
  `tessera-core`. It does not expose C functions yet.

Package metadata and the minimum Rust version are defined in the root
`Cargo.toml` and inherited by both crates. `rust-toolchain.toml` selects the
exact compiler version, Rust 1.98.1, with rustfmt and Clippy. With rustup
installed, these components are selected automatically; the first run may
download missing components. `Cargo.lock` is kept in Git. There are no
external crate dependencies.

Build and check Rust independently of PostgreSQL:

```sh
make rust          # Debug build
make rust-release  # Optimized build
make rust-check    # Formatting, Clippy, and debug/release tests
make rust-clean    # Remove Cargo build products
```

The static libraries are `target/debug/libtessera_capi.a` and
`target/release/libtessera_capi.a` on macOS and Linux. The crates do not yet
contain algorithms or functional tests; these checks validate the workspace
setup until implementations and their tests are added together.

The existing `make`, `make install`, `make installcheck`, and `make clean`
targets remain C-only and do not require Cargo. They do not link or install
the Rust library, and `make clean` leaves Cargo build products alone.

## Rust safety and errors

Unsafe code is limited to `tessera-capi` and future isolated SIMD modules.
Every unsafe block must have a `SAFETY` comment explaining its invariants.
Both crates inherit the workspace's `unsafe_op_in_unsafe_fn = "deny"` lint.

Rust must not call PostgreSQL, directly or through callbacks, or retain
`TupleTableSlot`, `MemoryContext`, or Datum pointers after returning to C.

The following rules apply when the first C entry points are added:

- Stack unwinding is allowed only within Rust. Both build profiles explicitly
  use `panic = "unwind"`; overriding this with `panic = "abort"` is unsupported.
- Entry points use `extern "C"`, not `extern "C-unwind"`. All potentially
  panicking work must run inside `catch_unwind` within the Rust entry point.
  A caught panic becomes an error in the future `TessStatus` interface.
  `extern "C"` alone does not recover from a panic: an escaping panic aborts
  the process.
- Expected errors use `Result` and status returns, not panics. Catching a panic
  does not undo mutations: partial outputs must not be returned, and affected
  state must be restored or made unusable. Any `AssertUnwindSafe` use must
  explain why state remains safe after unwinding.
- Resource cleanup, error handling, and dropping a caught panic's payload
  must not panic. Catching panics does not protect against aborts, memory
  corruption, or a second panic during unwinding.
- PostgreSQL's `ERROR` uses `siglongjmp`, not Rust unwinding, and is not caught
  by `catch_unwind`. C may call `ereport(ERROR)` only after Rust has returned;
  jumping over active Rust calls would bypass Rust resource cleanup.
- The library must not replace the global panic hook or call PostgreSQL from
  a hook. The hook runs before a panic is caught.

The first real C entry point must include panic-to-status handling and a test
that calls it from C with an injected panic. The test must verify error return,
resource cleanup, no partial output, and safe state after failure with both
debug and release libraries. `cargo test` alone does not verify the library's
panic strategy because the test harness handles that setting separately.
