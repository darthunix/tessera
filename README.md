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

## Rust development

The Rust libraries provide batch-processing primitives and adapters for
PostgreSQL types. They can be built and checked independently of PostgreSQL.
Install [rustup](https://rustup.rs/); the repository selects the required
toolchain automatically.

```sh
make rust          # Debug build
make rust-release  # Optimized build
make rust-check    # Formatting, Clippy, and debug/release tests
make rust-clean    # Remove Cargo build products
cargo doc --workspace --no-deps --open
```

API details, examples, and safety requirements live in the Rust documentation.

The existing C build, installation, and test targets remain independent of
Cargo.
