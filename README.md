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

| query | PostgreSQL | Tessera | speedup |
|---|---:|---:|---:|
| filter and count over 2 M rows | 36.3 ms | 9.0 ms | 4.0× |
| `GROUP BY` into 100 k groups over 2 M rows | 126.1 ms | 36.1 ms | 3.5× |
| hash join of 2 M rows with 100 k, count | 114.4 ms | 22.6 ms | 5.1× |
| hash join of 20 M rows with 1 M spilling to disk (`work_mem` 4 MB) | 2189 ms | 529 ms | 4.1× |
| `GROUP BY` of 20 M rows spilling to disk (`work_mem` 4 MB) | 2599 ms | 952 ms | 2.7× |
| `ORDER BY` of 2 M integers | 113.9 ms | 43.0 ms | 2.6× |
| top 10 of 2 M rows | 65.2 ms | 15.6 ms | 4.2× |
| `UNION ALL` of two filtered scans, aggregated | 92.4 ms | 23.5 ms | 3.9× |
| a month of dates out of 2 M rows through a BRIN index, aggregated | 2.06 ms | 0.63 ms | 3.3× |
| the same filter and count with two parallel workers | 18.5 ms | 7.8 ms | 2.4× |

Same PostgreSQL, same heap tables, same data, same machine (Apple M5 Pro,
PostgreSQL master): each query runs with `tessera.enable` on and off in one
session; the table shows medians of 31 runs (11 for the spilling ones).
Of the 190 cases of the [full run](docs/benchmarks/2026-10-01/README.md),
Tessera is slower in four: a sort feeding a window function (1.19 times
the core's time: PostgreSQL's window node stores every row the batch sort
serves it), planning (0.03 to 0.05 ms more per query), and `LIMIT 1` (3 µs
against 2). With parallel workers the batch plans gain less than the
core's: the median speedup of the filter family is 3.4× serial and 2.1×
with two workers. Results of the queries derived from TPC-H will join
these after the first measured run of [their harness](bench/tpch/README.md).

## Why trust the results

- **Compared with PostgreSQL's executor.** The regression suites compare
  about a thousand queries with Tessera on and off and require the same
  rows: types and NULLs, expressions, aggregates, joins of every kind,
  spilling, parallel plans, rescans. Each node's tests also cover early
  stops, empty input and errors.
- **Continuous integration** on every push: Linux on x86-64, where the
  scalar kernels run, and macOS on arm64, where the vector kernels do,
  against PostgreSQL built with assertions.
- **Rust kernels** carry property tests and a loom model of the shared
  hash table's concurrent protocol; the table's tests also pass under
  Miri, run by hand.
- **Not yet**: builds with sanitizers and randomly generated queries.

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

The join of the table above, as `EXPLAIN ANALYZE` shows it (counters
omitted):

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
runs with their commit and machine, and the plans of the cases cited here. The kernels' micro-benchmarks
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
