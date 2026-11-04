## Why

A batch node reads its child's batches through the runtime's input,
`TessInput`. After a selective filter, or the output of a join, the
batches arrive nearly empty: the scan's batch holds a page's rows, and
a filter that keeps 1 % of them hands on one or two. Every consumer
pays its fixed cost a batch: the kernels' calls, the masks, the
requests for columns, the evaluation of its expressions.

TPC-H at SF 1 shows the cost. Q14's filter keeps 76 000 rows of 6 M,
and its join and aggregate receive them in 41 000 batches, 1.8 rows a
batch. Run with wider ranges of dates, the same query shows what each
costs: the aggregate about 0.6 µs a batch against 27 ns a row, the join
about 0.5 µs a batch. Of Q14's 139 ms, about 45 go to the two nodes,
nearly all of it a batch's cost. Q06's aggregate gets 1.8 rows a batch
and Q15's 3.4. The index lookup of TessHashJoin suffers the same: it
gains when a batch's keys share the btree's descents, and Q5's outer
side comes 2.1 keys a batch, Q7's 4.6, Q4's 2.5 and Q21's one, so the
cost model leaves those joins to the core's nested loops, which take
46 to 121 ms of the queries.

## What Changes

- A consumer can ask its input to gather sparse batches, before any
  scan's first batch: `tess_input_set_gather`
  (`tessera/runtime_input.h`) turns it on or off, and
  `tess_input_gathered_rows` gives how many rows it copied. From a
  scan's second batch on, such an input copies the selected rows of
  the child's sparse batches,
  the columns of the consumer's request only, into full batches of 64
  rows of its own, or of the request's smaller `max_batch_rows`,
  splitting a child batch that does not fit. A child batch at least
  half full goes as it is, never copied, after the rows gathered
  before it; so does the first batch after a start or a rescan, so
  that a consumer that stops after its first batch reads no further
  than now.
- Only batches whose columns are stored values are gathered: a new
  field of `TessBatch`, `values_stored` (`tessera/batch.h`), says that
  reading a column evaluates no expression. Each producer sets it on
  each batch by what the batch's getter reads: the heap batch, the
  builder's and the index-only scan's batches, the outputs of TessAgg
  and TessSort, the batches TessGather receives from workers, and a
  hash join's compact, spilled and tail output are stored; a join's
  round takes its outer batch's flag, and TessGather's own rows and
  TessAppend's batches their child's; a projection's batch, which
  computes its expressions on request, is not. A copy therefore
  evaluates nothing the consumer would not have read.
- A tuple bound handed to TessHashJoin turns its gathering off, scan
  by scan, so that a limit a rescan computes again turns it off and on.
- Reading ahead past an early stop elsewhere (a join under `EXISTS`,
  a node below the one a limit reaches), and the count and order of a
  volatile function's calls, change as they do within a batch now: the
  maintainer took them on 2026-11-04 as limitations, which
  `docs/limitations.md` gains.
- A gathered batch answers a numeric column's decimals as the heap
  batch does; the reading of decimals from a column of numerics becomes
  a function of the runtime both share.
- TessHashJoin gathers its outer side, in both of its modes; the index
  lookup then looks the keys of a gathered batch up together. TessAgg
  gathers its input.
- The index lookup's cost model counts a gathered outer side's batches
  by the gathering's rules, over the binomial spread of rows it already
  assumes and through an append's parts as one stream, and adds a copy
  a copied row and column, its price
  `tessera.join_lookup_copy_cost`, fitted with the lookup's others by
  the calibration family `lookupcost`, which gains sparse outer sides.
- `EXPLAIN VERBOSE` shows `Gathered Rows` on a node whose input copied
  rows, the input's count folded into the node's counters.
- Tests: the input's C test (`test/tessera_input_test.c`): sparse,
  dense and mixed child batches, a batch split, the first batch, a
  batch without stored values, a smaller `max_batch_rows`,
  by-reference values, NULLs, decimals, a rescan, the end of the
  input, batches of 32 rows and of 31 and 33 in turn; and SQL suites
  joining and aggregating sparse inputs against the core's rows,
  through the index lookup too, with an expression that fails or
  counts its calls on rows the join drops, under TessGather in the
  leader too, over TessAppend, an index-only scan and a join's every
  kind of output, a limit right above a join, INTERSECT and EXCEPT.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

None: results stay. The runtime's input has no capability of its own
yet; `docs/runtime.md` describes it and takes the new call. The
requirements of the lookup's table (`key-filter`, `table-memory`) hold
for a gathered batch as for any other. Plans change where the lookup's
model now takes a lookup. The change declares `skip_specs: true`.

## Impact

- C API: `tess_input_set_gather` and `tess_input_gathered_rows` in
  `tessera/runtime_input.h`; `values_stored`, a field at the end of
  `TessBatch` in `tessera/batch.h`.
- Code: `runtime/input.c` and a file of the gathered batch,
  `runtime/heap_batch.c` (the decimals moved to the shared function),
  `nodes/hashjoin_begin.c`, `nodes/hashjoin.c`,
  `nodes/hashjoin_pairs.c`, `nodes/hashjoin_spill.c`, `nodes/agg.c`,
  `nodes/agg_spill.c`, `nodes/sort.c`, `nodes/gather.c`,
  `nodes/append.c`, `nodes/heapscan.c`, `nodes/join_planner.c`,
  `nodes/module.c`.
- Tests: `test/tessera_input_test.c`, `test/sql/input.sql`,
  `test/sql/index_join.sql`, `test/sql/agg.sql`, `test/sql/join.sql`.
- Benchmarks: `bench/pg/lookupcost.sql`, `bench/pg/indexjoin.sql`.
- Documents: `docs/runtime.md`, `docs/costs.md`, `docs/nodes.md`,
  `docs/limitations.md`.
- Runs: TPC-H serial and with two workers, every query's plan with its
  lookups; Q14's batches and nodes' times again; the families `win`,
  `join`, `indexjoin` and `tax`.
