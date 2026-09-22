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
how a running installation preloads the bridge and the modules in every
session, and a runnable example using independent producer and consumer
modules, and
the [node guide](docs/node.md) for the node registry and what every batch
node must do. The [function guide](docs/function.md) describes how batch
implementations of PostgreSQL functions are registered and called, the
[expression guide](docs/expr.md) how a node evaluates expressions and
filters over a batch through them, and the
[runtime guide](docs/runtime.md) the static library a node links for
building and passing batches. The [nodes guide](docs/nodes.md) describes the
module of Tessera's own batch nodes, `TessHeapScan`, `TessPack`,
`TessFilter` and `TessAgg`, serial and under PostgreSQL's parallel query,
and the
[node-writing guide](docs/writing-a-node.md) walks through building a node
of your own on the example of `TessLimit`.

## Getting started

Tessera builds against a PostgreSQL master installation with server headers,
PGXS and `pg_config`; it needs a C compiler, `make` and
[rustup](https://rustup.rs/), since `make` builds the kernels' Rust library
through Cargo with the toolchain the repository selects.

1. Build and install into that PostgreSQL, from the repository root:

   ```sh
   export PG_CONFIG=/path/to/postgresql/bin/pg_config
   make
   make install
   ```

   This installs the bridge with its extension files, the kernels, the
   runtime static library with the public headers, the nodes module and the
   example limit node. Clean and rebuild when switching installations.

2. Get a server that loads the modules in every session. Either add the
   preload line to your cluster's `postgresql.conf` and start it:

   ```
   session_preload_libraries = 'tessera, tessera_nodes, tessera_kernels, tessera_limit'
   ```

   (the bridge first, since the modules need it; `tessera_limit` is the
   example node and optional), or let the benchmark runner create a
   temporary cluster on port 5433 with that line and three tables of 2 M,
   250 k and 500 k rows:

   ```sh
   bench/pg/run.sh setup
   psql -h /tmp -p 5433 postgres
   bench/pg/run.sh stop       # afterwards: stops and deletes the cluster
   ```

3. In a database, create the extension once (the benchmark cluster has it
   already), then look at a plan:

   ```sql
   CREATE EXTENSION tessera;
   EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
   SELECT count(*), sum(c1) FROM bench_narrow WHERE c1 > 1000000;
   ```

   The plan reads `TessAgg → TessFilter → TessHeapScan`, each node with its
   batch counters: batches, pages, deformed and computed datums, kernel
   calls. `SET tessera.enable = off` gives the core's plan for comparison.
   With `max_parallel_workers_per_gather` above zero, on a table larger than
   8 MB, the same query runs in every worker under a `Gather`:
   `Finalize Aggregate → Gather → Parallel Custom Scan (TessAgg) → …`. What
   the nodes handle today (one table, int4 and int8 filters as batch chains
   with the rest row by row, `count` over any type, `sum` over int4, `min`
   and `max` over int4 and int8, a limit) is in the
   [nodes guide](docs/nodes.md).

4. Run the regression tests, thirty suites, against a running server
   (`PGPORT` and `PGHOST` in the environment) or in a temporary instance,
   as the [bridge guide](docs/bridge.md) shows:

   ```sh
   make installcheck
   ```

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
See the [benchmark guide](crates/tessera-capi/benches/README.md) for performance
checks.

The existing C build, installation, and test targets remain independent of
Cargo.
