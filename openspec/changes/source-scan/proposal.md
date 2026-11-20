## Why

Tessera reads only plain heap tables in batches. A foreign table is read
by its foreign data wrapper (FDW) row by row, whatever the wrapper could
give:

- A batch parent (a grouping, a sort, a join) puts `TessPack` over the
  core's `ForeignScan` and packs its rows into batches again.
- A `WHERE` clause of a foreign table stays with the core: the scan
  planner admits tables only, never a foreign one, the runtime's native
  scan (`tess_batch_scan_path`) asks the heap scan node alone, and
  `TessFilter` takes as its child only the heap scan or a pack over a
  sequential scan.
- A wrapper can already check clauses in its own rows, but there is no
  way for a batch source to tell `TessFilter` which clauses it checks
  itself, so that the filter checks only the rest.
- The source registry holds names only. Its header says so: no node
  reads a source through it.

A module that reads a columnar format can already build a batch node of
its own, add its path from its wrapper and check every clause itself,
and the batch parents read it. What it cannot do is share the clauses
with `TessFilter`: the filter takes no such child, and has no way to
learn which clauses the source checks and which it leaves, so each
source would have to repeat the filter's work, its order of clauses, its
fallback to rows and its `EXPLAIN`. The maintainer decided on 2026-11-20
to enter ClickBench with Tessera reading the benchmark's Parquet files
in place (`parquet-fdw`), that the Parquet FDW is the first source of a
planning contract written for it, and that the source filters. The
numbers behind the choice, from the run `clickbench-kh13CL` on the M5
Pro (10 M of ClickBench's rows in a heap table, 6.5 GB in memory, the
settings of ClickBench's PostgreSQL entry):

- The 43 queries took 21.4 s with Tessera, 40.5 s with the core and 53.4
  s with pg_duckdb over the same heap table.
- A count by a clause on a column behind the text columns took 0.97 of
  the core's time on column 41 and 0.99 on column 105, against 0.69 on
  column 1: walking each heap row to a far column costs Tessera what it
  costs the core.
- On ClickBench's main machine (32 GB) the heap table does not fit in
  memory: every heap entry there, PostgreSQL's and pg_duckdb's, takes
  about 258 s a query, the time to read the table from its disk.
- 26 of the 43 queries have a `WHERE` clause. A columnar source can
  check one on the form in which it keeps a column, a dictionary's
  values once instead of each row's, and decode the other columns only
  for the rows that pass.

A columnar source reads only the columns a query asks for, so both costs
go. This change gives such a source its way into Tessera's plans.

## What Changes

- A source in the registry gains an optional callback, `scan_rows`.
  Given the core's scan path of a relation that is not a plain heap
  table, in this change a foreign table's, and the relation's clauses,
  it returns the path of its own batch node that reads the relation, or
  `NULL`. The node is an ordinary batch node of the node registry.
- The source answers for each clause: it does not use it, it uses it
  only to skip rows that cannot pass, or it checks it exactly, so that
  every row it returns passes it. Tessera marks the clauses a source may
  use at all: those the core marks leakproof, which pass no value of the
  relation to a function that may reveal it, and those of the lowest
  security level, and never a volatile one nor one that holds a subquery
  run from within the clause, so that a source never runs a clause ahead
  of the security clauses that guard it, nor a volatile clause apart
  from the filter.
- The registry lists its sources in the order they were registered, so
  that the planner can ask each in turn.
- The scan planner offers a foreign table's scan path to the sources;
  the first path built wins. A sequential scan of a plain heap table
  still goes to the heap scan node alone, so heap plans do not change,
  and no batch parent asks a source: a parent reads a source's path when
  the planner chose it, and packs the wrapper's rows otherwise.
- The scan planner admits a foreign table that a source reads:
  `TessFilter` over the source's path with the clauses the source does
  not check exactly; the source's path alone when it checks them all or
  the relation has none; serial and partial, and `TessGather` over the
  partial path. A batch parent then reads a foreign table without
  `TessPack`.
- Each clause is checked once: by the source when it answers that it
  checks it exactly, otherwise by `TessFilter`, in batches or row by
  row, as over heap.
- A source's path costs what the source says, and `TessFilter` over it
  adds the cost of the clauses it checks, so that the planner weighs the
  source's batches against its wrapper's rows.
- A test source, an FDW and a batch node in a test module, serves
  generated rows and checks some clauses itself. A new SQL suite
  compares its plans and results with the same rows in a heap table:
  serially and in parallel, with the source checking all, some or none
  of the clauses, with clauses that run in batches and row by row, under
  the row security policy of a partitioned table over a foreign
  partition and behind a view with `security_barrier`, and with Tessera
  off.
- The guides: `docs/source.md` (the contract, with a full example),
  `docs/runtime.md`, `docs/nodes.md`, `docs/limitations.md`,
  `docs/bridge.md`, `docs/node.md`.

Not in this change:

- The Parquet FDW: `parquet-fdw`, which comes next.
- Join clauses checked by a source: a batch path is never parameterized,
  so the inner side of a nested loop parameterized by the outer row
  stays the core's.
- Columns in forms other than `Datum` arrays. The batch's contract stays
  as it is.
- Tables of another access method, index and bitmap scans of a source,
  and foreign tables read by a query level that writes or locks rows; a
  subquery planned on its own inside such a statement reads through a
  source, as it reads heap through the batch nodes.
- The compressed test table access method of `compressed-table-source`.

## Capabilities

### New Capabilities

- `source-scan`: the source registry, and the contract by which a module
  reads a relation that is not a heap table in batches for Tessera's
  nodes: what Tessera offers a source, the clauses it lets the source
  check, what the source returns and owes, and what Tessera plans over
  it.

### Modified Capabilities

None. A foreign partition under `TessAppend` reads through its source
the way a heap partition reads through the heap scan, and the
requirements of `partition-pruning` say nothing of the kind of a
partition.

## Impact

- Public headers: `include/tessera/source.h` (a field of `TessSource`,
  the clauses offered and the answers, a required operation of
  `TessSourceRegistryOps`, all under the ABI's size rule), and the
  comment of `TessNode.scan_rows` in `include/tessera/node.h`, and one
  call of the runtime in `tessera/runtime_api.h`,
  `tess_source_registry_check`, which checks the registry's table. The
  runtime's calls in `tessera/planner.h` do not change.
- Code: `bridge/source.c`, `runtime/api.c` (the check of the registry's
  table) and `nodes/scan_planner.c` (the walk over the sources, the
  filter's clauses). No kernel, crate or format changes.
- Tests: `test/tessera_source_test.c` and the suite
  `test/sql/source.sql` with its expected output, a new test module of
  the test source, a new suite `test/sql/source_scan.sql`, and the
  installed set in `test/installed/`, which builds the test source
  against the installed headers.
- Settings: none new. `tessera.enable` covers the sources.
- Specs: a new capability `source-scan` with its design and its
  architecture contract.
- Performance: no hot path. The sources are asked only for foreign
  tables, at planning; every existing suite keeps its plans.
- The roadmap: the change takes up the planning contract and the answers
  per clause that `compressed-table-source` carried, as a new entry
  `source-scan`, taken out of turn; `parquet-fdw` follows it, depends on
  it instead of on `compressed-table-source`, and lives in the
  repository as a module of its own.
