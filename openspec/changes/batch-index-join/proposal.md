## Why

The core's nested loops over an index, one lookup a row, are the
largest block left in TPC-H with Tessera on: Q5, Q7, Q8 and Q21 serial
and Q3 and Q8 with two workers look up `lineitem_pkey`, `orders_pkey`
and `customer_pkey` row by row, and Q4 and Q21 do it for `EXISTS` and
`NOT EXISTS`. A hash join over a full scan does not replace them: with
nested loops off, Q4 takes 3 times as long and Q21 2.3 times. A profile
puts most of a lookup in the btree's descent from its root; the
executor's work around a lookup is 1 to 2 % of a query. The core's
btree walks its leaves for a sorted array of keys (`= ANY`): the keys of
125 000 orders in arrays of 64 took a tenth of the descents, a third of
the buffer reads and 23 % less time than one lookup a row.

## What Changes

- TessHeapScan gets a lookup mode: it opens an index of its relation
  and, for each array of keys it is given, scans `key = ANY(array)` and
  puts the rows found into its batches, as its index mode does.
- TessHashJoin gets an index join mode for INNER and LEFT joins: for
  each outer batch with keys, it gives the batch's keys to the lookup
  mode below it, rescans its inner side, builds a table of the rows
  found and probes that batch. The keys go down by a call inside the
  `tessera_nodes` module, not by a parameter of the core's plan.
- The planner offers the mode where the inner side is a table with a
  btree index, not partial, whose first column is the inner key of an
  equality the index's operator family serves, of any key type the
  join takes, and where the most frequent key's rows for a batch fit a
  quarter of `hash_mem`, costed per outer batch; under a Gather each
  participant looks up the keys of its own batches.
- EXPLAIN shows the lookups: `Inner Lookups` on the join, the index and
  the key on the scan, and with `ANALYZE` the lookups and keys, and the
  memory a batch's table took past `hash_mem`.
- A benchmark family of index nested loops.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

- `table-memory`: a table built per outer batch from an index lookup
  never spills; it counts its memory past `hash_mem` as overrun.
- `key-filter`: such a join builds no Bloom filter.

The hash join and the heap scan have no capability of their own yet;
their modes are described in `docs/nodes.md`.

## Impact

- Code: `nodes/heapscan.c` (the lookup mode), `nodes/hashjoin*.c` (the
  table per outer batch), `nodes/join_planner.c` (the path and its
  cost), `nodes/scan_planner.c` (the scan's eligibility and its filter,
  shared with the lookup path).
- No change of the C API, the table or the kernels.
- Tests: `test/sql/join.sql`, `test/sql/parallel.sql`.
- Documents: `docs/nodes.md`; `bench/pg/`.
- Runs: TPC-H Q3, Q5, Q7, Q8 and Q21 serial and with two workers.
