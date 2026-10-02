# Tessera

Batch execution for PostgreSQL, built as extensions: analytical queries run
over ordinary heap tables in batches of 64 rows, without patching
PostgreSQL or copying the data anywhere.

- **No fork.** Three modules loaded into a stock PostgreSQL server.
- **No data migration.** The batch nodes read the heap pages and indexes
  PostgreSQL already has, under the query's snapshot.
- **Mixed plans.** Batch nodes take the parts of a plan they support;
  PostgreSQL's own nodes run the rest, in the same plan.
- **Composable.** Other extensions add their own batch nodes, sources and
  functions through Tessera's bridge, a C API.

## How much faster

Same PostgreSQL, same heap tables, same data, same machine: every case
runs with `tessera.enable` on and off in one session, and the ratio of
the two medians is the speedup. The exact numbers live with each run,
since they change with every series; roughly, on an Apple M5 Pro with
PostgreSQL master and the data in shared buffers:

- **Filters and aggregates over large tables** (a count or a sum under a
  condition, `GROUP BY` into a few or many groups): 1.5 to 5 times
  faster, 3 in the middle; less over a wide row, where deforming it is
  the work of both modes.
- **Hash joins by integer keys**, inner, outer, semi and anti: 3 to 5
  times; spilling to disk 2 to 8; a join that prunes the partitions of
  its outer side by the keys it built, up to 15.
- **Sorting, top-N and `DISTINCT`**: 2 to 4 times by integers and dates,
  up to 7 for `DISTINCT` over many values, barely faster by text under
  a collation, whose comparisons are the core's.
- **Set operations** (`UNION`, `INTERSECT`, `EXCEPT`): 2 to 7 times.
- **Reads through indexes** (bitmap, index, index-only, BRIN): 1 to 3.5
  times, since the index does most of the work in both modes.
- **The queries derived from TPC-H** at scale factor 1: every query
  faster, from 1.1 to 3 times, 1.7 times on the geometric mean; two of
  the 22 run past the time limit in both modes without indexes on the
  foreign keys.
- **Where Tessera is slower**: a sort feeding a window function (about
  1.2 times the core's time: PostgreSQL's window node stores every row
  the batch sort serves it), planning (a few hundredths of a millisecond
  more per query), `LIMIT 1` (microseconds); with parallel workers the
  batch plans gain less than the core's, so the speedup with two workers
  is about two thirds of the serial one.

The runs: the [full run of the benchmark families](docs/benchmarks/2026-10-01/README.md)
(190 cases, medians of 31 runs) and the [run of the queries derived from
TPC-H](docs/benchmarks/2026-10-02-tpch/README.md) (SF 1, 11 pairs in
alternating order); [`docs/benchmarks`](docs/benchmarks/README.md) lists
them, newest first.

## Why trust the results

- **Compared with PostgreSQL's executor.** The regression suites compare
  about a thousand queries with Tessera on and off and require the same
  rows: types and NULLs, expressions, aggregates, joins of every kind,
  spilling, parallel plans, rescans. Each node's tests also cover early
  stops, empty input and errors.
- **Random queries.** Every push runs 3000 generated queries, plus the
  seeds that once found a bug, with Tessera on and off, and fails on any
  difference ([tessera-crosscheck](tools/tessera-crosscheck/README.md)).
  The queries mix joins of every kind, `EXISTS`, grouping, set
  operations, limits and parallel plans over data full of NULLs and edge
  values. The tool found five bugs before this gate went up, among them a
  planner crash and wrong rows from an anti join; each fix carries its
  case into the suites.
- **Continuous integration** on every push: Linux on x86-64, where the
  scalar kernels run, and macOS on arm64, where the vector kernels do,
  against PostgreSQL built with assertions. The suites also run with the
  debug build of the Rust kernels, under AddressSanitizer and
  UndefinedBehaviorSanitizer, and Tessera's C builds with warnings as
  errors.
- **Rust kernels** carry property tests, a loom model of the shared hash
  table's concurrent protocol, and Miri over the tests of their unsafe
  code, all in CI. Outside tests, clippy refuses `unwrap`, `expect` and
  `panic!`, and the boundary with C turns a panic into an error.
  [cargo-mutants](https://mutants.rs) checks that the tests notice a
  change to the lines a pull request touches in the Rust crates.

## What runs in batches

| | in batches | stays with PostgreSQL |
|---|---|---|
| scans | heap tables: sequential, bitmap, index, index-only, BRIN | other access methods, foreign tables, `TABLESAMPLE` |
| conditions | comparisons, arithmetic, `IN`, `BETWEEN`, `LIKE`, `CASE` over integers, numeric, dates and text; other functions row by row inside the batch filter | — |
| aggregation | `count`, `sum`, `avg`, `min`, `max` by kernels, other aggregates of PostgreSQL's own functions; `GROUP BY` up to 16 keys; `DISTINCT`; spilling | grouping sets, ordered-set aggregates, `ORDER BY` inside an aggregate, aggregates of other extensions in C |
| joins | hash joins: inner, left, right, full, semi, anti; spilling; one shared table in parallel plans; partitions pruned by the join's keys | nested loop, merge join, right semi and right anti joins |
| sorting | `ORDER BY`, top-N, external sort | sorts inside a plan (under a merge join or a window function), `WITH TIES`, incremental sort |
| set operations | `UNION ALL`, `UNION`, `INTERSECT`, `EXCEPT` | — |
| parallel query | Tessera's own `Gather` and `Gather Merge` | — |
| window functions | — | all |
| statements | `SELECT` | the scans of `INSERT`, `UPDATE`, `DELETE` and `FOR UPDATE` |

[What stays with the core](docs/limitations.md) gives the details, each
with the regression case that shows it.

## How it fits in a plan

A hash join of 2 M rows with 100 k, counted, as `EXPLAIN ANALYZE` shows
it (counters omitted):

```
Custom Scan (TessAgg) (actual rows=1.00 loops=1)
  ->  Custom Scan (TessHashJoin) (actual rows=1846154.00 loops=1)
        Hash Cond: (f.fk = d.id)
        ->  Custom Scan (TessHeapScan) on public.bench_fact f (actual rows=2000000.00 loops=1)
        ->  Custom Scan (TessHeapScan) on public.bench_dim d (actual rows=100000.00 loops=1)
```

Batch nodes are `CustomScan` nodes. Where a batch node's input is a node
of PostgreSQL's, `TessPack` gathers its rows into batches; where a
PostgreSQL node reads a batch node, the batch node serves it rows, one per
call, from its batches:

```
PostgreSQL node             reads rows
    ↑ rows
batch node → batch node     pass batches, 64 rows each
    ↑ batches
TessPack                    packs rows into batches
    ↑ rows
PostgreSQL node
```

## Reliability

- **Read-only.** Batch nodes run in `SELECT` queries, and in subqueries a
  writing statement plans on its own; the scans of statements that write
  or lock rows stay PostgreSQL's.
- **PostgreSQL decides visibility.** The batch scan reads the pages the
  heap access method reads, under the query's snapshot.
- **What is not supported stays with PostgreSQL**, per node and per
  expression, rather than failing.
- **Cancellation**, by a cancel request or `statement_timeout`, reaches
  the batch nodes' own loops: they check for interrupts as PostgreSQL's
  nodes do.
- **Memory** follows `work_mem` and `hash_mem_multiplier`: hash tables and
  sorts spill to temporary files under `temp_file_limit`, and a damaged
  temporary file is an error (`XX001`), not a wrong result.
- **Errors** of the kernels are PostgreSQL's own: the same overflow and
  division errors with the same codes.

## Quick start

Tessera builds against PostgreSQL master with server headers, PGXS and
`pg_config`; it needs a C compiler, `make` and [rustup](https://rustup.rs/),
which installs the Rust toolchain the repository selects.

```sh
export PG_CONFIG=/path/to/postgresql/bin/pg_config
make
make install       # make clean first when switching installations
```

Preload the modules, bridge first, and restart the server (the
[bridge guide](docs/bridge.md) shows `session_preload_libraries` instead):

```
shared_preload_libraries = 'tessera, tessera_nodes, tessera_kernels'
```

Then compare a plan with PostgreSQL's own:

```sql
CREATE EXTENSION tessera;
EXPLAIN ANALYZE SELECT count(*), sum(x) FROM t WHERE x > 1000;
SET tessera.enable = off;   -- the same query, PostgreSQL's plan
EXPLAIN ANALYZE SELECT count(*), sum(x) FROM t WHERE x > 1000;
```

`bench/pg/run.sh setup` creates a temporary cluster on port 5433 with the
modules preloaded and the benchmark tables; `bench/pg/run.sh stop` removes
it.

## Benchmarks

[`bench/pg`](bench/pg/README.md) holds the PostgreSQL-level benchmarks:
families of queries, each run with Tessera on and off in one backend over
the same data, each statement prepared once per mode, with warm-up runs,
medians of 31 runs and the plans of both modes recorded.
[`bench/tpch`](bench/tpch/README.md) runs the 22 queries derived from TPC-H
on a cluster of its own with one command, `cargo tpch`: it checks every
answer against the published one and Tessera on against off, records which
Tessera nodes each plan has, and times both modes in alternating pairs.
[`docs/benchmarks`](docs/benchmarks/README.md) keeps the summaries of full
runs with their commit and machine, and the plans of the cases cited here.
Two families, `scancost` and `joincost`, calibrate the planner's cost
models rather than compare ([docs/costs.md](docs/costs.md)). The kernels' micro-benchmarks
count instructions, cycles and branches on macOS
([`crates/tessera-capi/benches`](crates/tessera-capi/benches/README.md)).

## Status

Tessera is under active development and is not ready for production use.
It builds against PostgreSQL master only.

## For extension developers

- [Bridge](docs/bridge.md): the public C API, ownership rules, how a
  server preloads the bridge and the modules, and an example of
  independent producer and consumer modules.
- [Node contract](docs/node.md): the node registry and what every batch
  node must do; [writing a node](docs/writing-a-node.md) walks through one.
- [Functions](docs/function.md) and [expressions](docs/expr.md): batch
  implementations of PostgreSQL functions and how nodes evaluate
  expressions through them.
- [Runtime](docs/runtime.md): the static library a node links to build,
  pass and serve batches.
- [Nodes](docs/nodes.md): Tessera's own nodes, serial and parallel;
  [table](docs/table.md) and [spill](docs/spill.md): the hash table of
  joins and grouping, and how it goes to temporary files.
- [Planning costs](docs/costs.md): how the planner prices Tessera's
  paths against the core's, the models of each node's time, their
  parameters and the families that calibrate them on a machine.

Run the regression suites against a running server, or in a temporary
instance as [CONTRIBUTING.md](CONTRIBUTING.md) shows:

```sh
make installcheck
```

The Rust libraries build and test without PostgreSQL:

```sh
make rust-check    # formatting, Clippy, debug and release tests
cargo doc --workspace --no-deps --open
```

Property tests (proptest, with the strategies of `crates/tessera-testing`)
draw new cases on every run; `PROPTEST_CASES=10000` runs more of them and
`PROPTEST_RNG_SEED=<n>` repeats one run. A failing case shrinks to a small
one and is kept in `<test file>.proptest-regressions`, committed so that
every later run replays it first.

## The name

A tessera is one of the small tiles of an ancient mosaic: small,
independently tested modules that together make a batch executor for
PostgreSQL, in the spirit of DataFusion's composable engine.
