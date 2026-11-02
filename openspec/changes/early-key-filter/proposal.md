## Why

A hash join hands its Bloom filter of keys to a TessFilter below it only
when that TessFilter has a clause that runs row by row, and the join
builds a filter only for a table of 4096 records at least. TPC-H Q19
builds a table of 485 parts and Q21 one of 411 suppliers; the lineitem
scans below both are TessFilters of batch clauses alone, which run on
all 6 M rows, and the join then drops nearly every row they kept. A
prototype that runs the filter first in such a TessFilter, with a
filter for the small tables (`tessera.join_bloom_ratio` at 1), takes Q19
from 265 to 97 ms and Q21 from 320 to 238 ms, serial, in one server.

## What Changes

- A TessFilter whose clauses all run in batches takes a join's filter,
  and checks it before its first clause, on the key columns only: the
  clauses then read their columns for the rows that passed. A TessFilter
  with a row-wise clause keeps the filter where it checks it now, before
  its first row-wise clause.
- A table of fewer than 4096 records gets a filter too, under the same
  share of rows found, only to hand down: the join does not check it
  itself, since a probe of such a table stays in the cache. When the
  child refuses it, the join drops it. A shared table of that size
  allocates the filter's words with its directory.
- EXPLAIN keeps its lines: a small table's filter that the child
  refused is dropped before it is counted in `Bloom Filters`.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

- `key-filter`: who hands a filter down (a small table's filter), who
  takes it (a TessFilter of batch clauses alone) and where the child
  checks it (before its first clause when all run in batches).

## Impact

- Code: `nodes/filter.c` (where the filter is checked),
  `nodes/hashjoin.c` and `nodes/hashjoin_shared.c` (a small table's
  filter), `nodes/costs.h`. No change of the C API: TessFilter checks
  the filter itself before it calls `tess_qual_apply`, and
  `TessKeyFilter` keeps its fields.
- Tests: `test/sql/join.sql`, where "A TessFilter of batch clauses
  alone has nothing to save" turns into its opposite, and the parallel
  cases of the shared filter.
- Documents: `openspec/specs/key-filter/`, `docs/nodes.md`.
- Runs: TPC-H Q19 and Q21 serial and with two workers; the join
  family's Bloom cases; a case of its own for the worst order.
