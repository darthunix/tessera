## MODIFIED Requirements

### Requirement: The chunks of a table in memory
A `TessHashJoin` building a table in memory, a `TessAgg` grouping in
memory, and the set of each DISTINCT aggregate of a `TessAgg` SHALL
take a first chunk of 64 kB and every later chunk of an eighth of
`hash_mem`, no less than 64 kB and no more than 1 MiB, rounded down to
a multiple of 8. Each process of a parallel build SHALL take its own
first chunk of 64 kB. A chunk of a join's values SHALL be at least as
large as the value it is opened for, and a value larger than a quarter
of 1 MiB SHALL take a chunk of its own size.

#### Scenario: A DISTINCT set under a small limit
- **WHEN** a grouping with `count(DISTINCT v)` of an integer `v`, by
  one integer key of one group, holds 3000 distinct values at
  `work_mem` = 384 kB and `hash_mem_multiplier` = 1
- **THEN** its set takes two chunks of 64 kB, and its Memory Usage is
  245 kB: the group's table, those chunks, and the set's old and new
  index at its last regrow
- **Verified by:**
  `test/sql/agg.sql::spec: table-memory/A DISTINCT set under a small limit`

#### Scenario: A table rebuilt at a lower limit
- **WHEN** a join's table of 3000 integer keys is built at `work_mem` =
  8 MiB and built again, in the same query, after `work_mem` fell to
  512 kB, with `hash_mem_multiplier` = 1
- **THEN** the second build's chunks take the length of the new limit,
  and it stays in memory
- **Verified by:**
  `test/sql/join.sql::spec: table-memory/A table rebuilt at a lower limit`

### Requirement: What a spill counts and holds past the limit
A `TessHashJoin` or a `TessAgg` that spills SHALL count in the memory it
decides by, and in its Memory Usage, every region its spill holds, each
by its capacity and once: the chunks and the tails of its partitions;
the write buffer and the block lists of each set it writes; a block and
a page of a block list for each open reader; for a partition a join
reads in pieces, the marks of its outer rows that matched and the block
that starts its next piece; and the writers of a grouping's rows, and
of a DISTINCT set's pairs, while their level is written. A finished
set SHALL keep no block list in memory: it reads its lists from the end
of its file. Past `hash_mem` a spill MAY hold only what no send, split
or piece lowers: the tail chunk of each partition, a write buffer, a
block and a page for each reader, and the marks of matched rows, which
grow with a partition's outer rows. When PostgreSQL cannot give it the
memory it asks for, the query SHALL fail with PostgreSQL's
out-of-memory error, SQLSTATE 53200.

#### Scenario: A set's memory as it writes, finishes and is read
- **WHEN** a serial set packs a chunk longer than its write buffer, is
  finished, and a reader of the chunk's partition opens and closes
- **THEN** the set counts its buffer, the chunk packed aside and the
  partition's list while it writes, nothing once finished, and a block
  and a page of the list while the reader is open
- **Verified by:** `test/sql/spill.sql::tessera_test_spill_memory`

#### Scenario: A spill past the limit
- **WHEN** a RIGHT join at `work_mem` = 64 kB, whose chunks hold a few
  records each, splits its partitions into levels below
- **THEN** it splits 22 partitions and joins them in 2 extra passes,
  its finished levels keeping no list and nothing packed aside, its
  Memory Usage is over `hash_mem`, and its rows are the core's
- **Verified by:**
  `test/sql/join.sql::spec: table-memory/A spill past the limit`

#### Scenario: Memory that runs out
- **WHEN** PostgreSQL cannot give a spill the memory it asks for
- **THEN** the query fails with SQLSTATE 53200
- **Verified by:** review only — no test makes the server run out of
  memory at a chosen allocation; every region of a spill comes from a
  memory context of PostgreSQL, which raises this error, and the crate
  allocates nothing

## ADDED Requirements

### Requirement: A DISTINCT set past the limit
A `TessAgg` grouping with `GROUP BY` SHALL send to disk the pairs of
group and value of a DISTINCT aggregate whose argument is a
`smallint`, an `integer`, a `bigint`, a `date`, a `boolean`, a
`timestamp` or a `timestamptz`, once the node's memory passes seven
eighths of `hash_mem`, and SHALL count each pair in its group once.
Where the planner gave the node the grouping although its estimate of
the pairs did not fit `hash_mem`, that memory SHALL count the groups,
while the input is read, by the most of their bytes and of the
planner's estimate of them. Its rows SHALL be the core's at any
`hash_mem`. EXPLAIN ANALYZE SHALL count the partitions and the disk of
the pairs in `Batches` and `Disk Usage`, and its VERBOSE form SHALL
show `Spilled Pairs`, the pairs written. For a grouping the planner
gave the node although its estimate of the pairs did not fit, while
its groups and their states take no more than their estimate, its
Memory Usage SHALL stay within `hash_mem`, but by what one batch adds
past seven eighths, by the chunk each partition of a level fills and
the level's write buffer, by a set of fewer than 64 pairs, and by the
pairs of a partition that no bit of their hash is left to split.

#### Scenario: Pairs past the limit
- **WHEN** a grouping counts `count(DISTINCT v)` of a `bigint` by an
  `integer` key of 100 groups at `work_mem` = 4 MB and
  `hash_mem_multiplier` = 1, its pairs several times `hash_mem`
- **THEN** its plan is `TessAgg`'s, its `Batches`, `Disk Usage` and
  `Spilled Pairs` are above 0, its Memory Usage is within 4 MB and the
  512 kB its level's tail chunks and write buffer take, and its rows
  are the core's
- **Verified by:**
  `test/sql/agg.sql::spec: table-memory/Pairs past the limit`

#### Scenario: A partition read back past the limit
- **WHEN** the pairs of one partition written by a set are more than
  `hash_mem` holds
- **THEN** `Batches` is above 32, the partitions of one set's first
  level, and the rows are the core's
- **Verified by:**
  `test/sql/agg.sql::spec: table-memory/A partition read back past the limit`

#### Scenario: Aggregates through the spill
- **WHEN** `count`, `sum`, `avg`, `min` and `max` with DISTINCT of
  integers, two DISTINCT aggregates of one grouping, one with `FILTER`,
  NULL arguments and a generic aggregate beside them go through a spill
  of pairs
- **THEN** the rows are the core's
- **Verified by:**
  `test/sql/agg.sql::spec: table-memory/Aggregates through the spill`

#### Scenario: Groups thought more than they are
- **WHEN** a grouping by two keys that depend on each other, with
  `count(DISTINCT v)` of a `bigint`, has groups the planner estimates
  at three quarters of `hash_mem` but few in fact, and its pairs fit by
  the estimate and in fact
- **THEN** its plan is `TessAgg`'s, its EXPLAIN ANALYZE shows no
  `Batches`, and its rows are the core's
- **Verified by:**
  `test/sql/agg.sql::spec: table-memory/Groups thought more than they are`

### Requirement: The groupings with DISTINCT the node takes
The planner SHALL offer `TessAgg` for a grouping with a DISTINCT
aggregate only when its estimate of the groups before HAVING, each with
the states of its aggregates, fits `hash_mem`, and either its estimate
of the pairs of all its DISTINCT aggregates together, with the
dictionaries of their values, fits `hash_mem` too, or the
grouping has `GROUP BY`, every DISTINCT argument is of a type of the
requirement "A DISTINCT set past the limit", and the groups take at
most half of `hash_mem`.

#### Scenario: A generic aggregate beside DISTINCT
- **WHEN** a grouping of `sum` and `avg` of a `smallint` and
  `count(DISTINCT v)` of a `bigint` has groups and pairs that fit
  `hash_mem`
- **THEN** its plan is `TessAgg`'s, and its rows are the core's
- **Verified by:**
  `test/sql/agg.sql::spec: table-memory/A generic aggregate beside DISTINCT`

#### Scenario: Groups past the limit by the estimate
- **WHEN** the planner's estimate of the groups of a grouping with
  `count(DISTINCT v)` and a generic aggregate, with their states,
  passes `hash_mem`, though its pairs fit
- **THEN** its plan is the core's
- **Verified by:**
  `test/sql/agg.sql::spec: table-memory/Groups past the limit by the estimate`

#### Scenario: Pairs past the limit by the estimate
- **WHEN** the planner's estimate of a grouping's pairs of an
  `integer` key and a `bigint` passes `hash_mem` and its groups take
  less than half of it
- **THEN** its plan is `TessAgg`'s
- **Verified by:**
  `test/sql/agg.sql::spec: table-memory/Pairs past the limit by the estimate`

#### Scenario: Groups past half the limit
- **WHEN** the planner's estimate of the pairs of a grouping with
  `count(DISTINCT v)` of a `bigint` passes `hash_mem`, and its groups
  take more than half of it
- **THEN** its plan is the core's
- **Verified by:**
  `test/sql/agg.sql::spec: table-memory/Groups past half the limit`

#### Scenario: Text sets past the limit together
- **WHEN** the planner's estimates of the pairs of two
  `count(DISTINCT)` of `text` arguments by group each fit `hash_mem`
  but pass it together
- **THEN** its plan is the core's
- **Verified by:**
  `test/sql/agg.sql::spec: table-memory/Text sets past the limit together`
