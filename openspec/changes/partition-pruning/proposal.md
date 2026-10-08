## Why

Tessera prunes partitions while a query runs in two ways. TessAppend,
which reads the children of an `Append` in batches, prunes them by the
query's parameters as the core's `Append` does. A hash join prunes the
partitions of its outer side by the keys of the table it built, which
the core does not do at all. Both are described only in `docs/nodes.md`,
`docs/costs.md` and the comments of `nodes/append.c`,
`nodes/hashjoin.c` and `nodes/join_planner.c`, and no requirement ties
them to a test. Comparing the texts with the code and the tests found:

- With `parallel_leader_participation` off, EXPLAIN ANALYZE shows
  `Subplans Removed by Join: 0` over a join that removed three of four
  partitions: the line shows the leader's own count, and only the
  workers pruned.
- In JSON and the other formats, TessAppend leaves out
  `"Subplans Removed": 0`, which the core's `Append` shows, though the
  text promises the line "as for the core's `Append`".
- The planner's expectation for a hash partitioning goes through the
  steps of a btree one: it reads the statistics through the hash
  equality as if it were "less than", and takes the hash equality for
  strategy 1, "less than". Nothing comes of it only because PostgreSQL
  carries an equality with a constant across the join itself; the text
  says that a hash partitioning expects nothing.
- `docs/nodes.md` says that the cost template reads the partitions left;
  the code scales the outer child's cost by them, and the join's own
  terms still count the rows of every partition.
- The tests do not show what the text says they show: the rescan that
  builds its table anew finds no inner row, so it would pass without
  the new keys; the spilling table is never shown to spill; the plan
  without the leader compares results only; the materialized CTE most
  likely runs the core's `Append`; no parallel case prunes by both a
  stable function and an initplan.
- No test shows: the bound of 1024 rows with a key; a RIGHT join whose
  keys are all NULL; a join of two keys; the join's pruning together
  with the node's own; `enable_partition_pruning` off; a parallel plan
  left with no partition or pruned by a generic plan's `$1`; a rescan
  that changes other parameters only; a plain table in a `UNION ALL`
  of partitions.
- Smaller texts: the node says a wrong shift of range table numbers is
  the core's error, where the node's own check meets it first; a
  comment says the core registers only an `Append`'s pruning, where it
  registers a `MergeAppend`'s too; a stray comment in `hashjoin.c`; the
  conditions on the partition key (its first column, a hash key of one
  column) and on `enable_partition_pruning` are in no text.

## What Changes

- A new capability `partition-pruning`: pruning when a node starts and
  while it runs, the relations a node prunes, a parallel plan, when a
  hash join prunes its outer side and by which keys, rescans and
  parallel joins, what EXPLAIN shows, the planner's expectation, and a
  pruning join under a gather. Ten requirements; every scenario names
  its test.
- The text about pruning moves from `docs/nodes.md` to
  `openspec/specs/partition-pruning/design.md`, and is then written
  anew.
- Corrections, each with its test:
  - `Subplans Removed by Join` counts every participant's pruning, not
    the leader's alone;
  - TessAppend shows `Subplans Removed` in every format but text even
    when it is 0, as the core's `Append`;
  - a hash partitioning gives no expectation by an explicit rule, not
    by the numbers of strategies;
  - the comments and the guides follow the code.
- Tests of the promises without one, and the weak tests made to show
  what they claim.

## Capabilities

### New Capabilities
- `partition-pruning`: pruning the partitions of a relation while a
  query runs, by its parameters and by a hash join's keys.

### Modified Capabilities

## Impact

- EXPLAIN: `Subplans Removed by Join` in a parallel plan without the
  leader; `"Subplans Removed": 0` in JSON, XML and YAML. Expected
  outputs of the union and join suites change.
- Code: `nodes/append.c`, `nodes/join_planner.c`, `nodes/hashjoin.c`.
- Documents: `docs/nodes.md`, `docs/costs.md`.
- Tests: `test/sql/union.sql`, `test/sql/join.sql`.
- Roadmap: the finding the change does not settle, the join's own cost
  terms over every partition, goes to "Not placed" beside "The costs of
  join pruning".
