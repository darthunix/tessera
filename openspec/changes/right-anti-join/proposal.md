## Why

For `NOT EXISTS` the core plans an anti join in two directions: `Hash
Anti Join`, which builds the side the subquery reads and probes with the
rows it keeps, and `Hash Right Anti Join`, which builds the kept side
and probes with the other. TessHashJoin takes only the first. Where the
kept side is small and the other large, the core's right anti join wins
and the probe side is read row by row: in TPC-H Q22 it joins 1.5 M
orders against 19 000 customers, 77 of the query's 159 ms serial.

## What Changes

- TessHashJoin takes `JOIN_RIGHT_ANTI`. It builds the kept side, probes
  with the other, marks the records that find a pair, and after the
  probe returns the records left unmarked: the tail its RIGHT join
  already has, without returning the pairs.
- The planner offers it as it offers RIGHT: serial, and under a Gather
  with a shared table only. Like RIGHT, it hands its Bloom filter to the
  probe side's scan and prunes a partitioned probe side by its keys,
  since an outer row without a pair leaves it. Its cost counts the pairs the probe finds and the
  columns of the records it returns.
- EXPLAIN shows `Join Type: Right Anti`.
- A benchmark case `right_anti` in the join family.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

- `key-filter`: RIGHT ANTI joins hand their Bloom filter down, as RIGHT
  joins do.
- `partition-pruning`: RIGHT ANTI joins prune their outer side, as
  RIGHT joins do.

The hash join itself has no capability yet; its kinds are described in
`docs/nodes.md`.

## Impact

- Code: `nodes/join_planner.c`, `nodes/hashjoin_begin.c`,
  `nodes/hashjoin_pairs.c`, `nodes/hashjoin.c`.
- Tests: `test/sql/join.sql`, `test/sql/parallel.sql`; plans of
  existing `NOT EXISTS` queries may change in their expected outputs.
- Specs: `key-filter` and `partition-pruning` name RIGHT ANTI among the
  joins that drop an outer row without a pair.
- Documents: `docs/nodes.md`; `bench/pg/join.sql`.
- Runs: TPC-H Q22 serial and with two workers; the join family.
