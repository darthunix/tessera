## Purpose

The memory a join's or a grouping's hash table in memory takes, as
seen from SQL: the length of its chunks, what EXPLAIN's Memory Usage
of the node counts, how a serial build keeps its index within
`hash_mem`, how large a grouping's first index is, and the error at
the most chunks.

## ADDED Requirements

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
  `work_mem` = 256 kB and `hash_mem_multiplier` = 1
- **THEN** its set takes two chunks of 64 kB, and its Memory Usage is
  245 kB: the group's table, those chunks, and the set's old and new
  index at its last regrow
- **Verified by:**
  `test/sql/agg.sql::spec: table-memory/A DISTINCT set under a small limit`

### Requirement: What Memory Usage counts
The Memory Usage that EXPLAIN ANALYZE shows for a `TessHashJoin` or a
`TessAgg` SHALL be the most memory its tables took at once in one
process: their chunks and indexes, both indexes while one replaces the
other, the sets of its DISTINCT aggregates from their first index on,
the chunks a grouping merges from a partition read back, and what the
node counted of its spill. In a parallel plan it SHALL be the sum of
each process's most, which need not have come at the same time.

#### Scenario: A regrow is counted with both indexes
- **WHEN** a DISTINCT set's index is rebuilt larger while another set
  is alive, and that is the node's highest moment
- **THEN** Memory Usage is the bytes of that moment, the old and the
  new index included
- **Verified by:**
  `test/sql/agg.sql::spec: table-memory/A regrow is counted with both indexes`

#### Scenario: Groups read back and merged
- **WHEN** a grouping reads a partition back, merges it and rebuilds
  its index larger, in a build with assertions
- **THEN** at every note of its memory the node's count of its table
  equals the chunks it owns and the index it has
- **Verified by:**
  `test/sql/agg.sql::spec: table-memory/Groups read back and merged`

#### Scenario: DISTINCT over no rows
- **WHEN** a grouping with two DISTINCT aggregates reads no rows
- **THEN** Memory Usage counts the first index of each set
- **Verified by:**
  `test/sql/agg.sql::spec: table-memory/DISTINCT over no rows`

### Requirement: A serial build keeps its index within the limit
A `TessHashJoin` that builds its table in one process SHALL start to
spill before a batch whose records, with the chunks and values it has,
room for two more chunks, and the index its records would need, would
pass `hash_mem`. A build SHALL stay in memory while all of these fit.

#### Scenario: A build whose index passes the limit
- **WHEN** a serial join builds 420 000 rows of one integer key at
  `hash_mem` = 16 MiB
- **THEN** it spills, its Memory Usage is within `hash_mem`, and its
  rows are the core's
- **Verified by:**
  `test/sql/join.sql::spec: table-memory/A build whose index passes the limit`

#### Scenario: A build that fits with its index
- **WHEN** a serial join builds 250 000 rows of one integer key at
  `hash_mem` = 16 MiB
- **THEN** it stays in memory, and its rows are the core's
- **Verified by:**
  `test/sql/join.sql::spec: table-memory/A build that fits with its index`

### Requirement: The first index of a grouping
A `TessAgg` grouping in memory SHALL size its first index for the
planner's estimate of its groups, at most for as many groups as fill,
at 4 bytes a bucket, a quarter of `hash_mem` with buckets rounded up
to a power of two, and at least for the table's least capacity.

#### Scenario: A limit that is not a power of two
- **WHEN** a grouping whose estimate is far above the cap starts over
  no rows at `hash_mem` = 2, 3 and 4 MiB, and one whose estimate is
  below the cap and one whose estimate is below the least capacity
  start at 3 MiB
- **THEN** the buckets of the first three take 512 kB, 512 kB and
  1 MiB, and the others the estimate's and the least capacity's
- **Verified by:**
  `test/sql/agg.sql::spec: table-memory/A limit that is not a power of two`

### Requirement: The most chunks of a table
A `TessHashJoin` or a `TessAgg` SHALL fail with SQLSTATE 54000 before
one of its tables would hold more than 32768 chunks, when it appends,
when it adopts chunks it did not allocate, and when a grouping merges
groups read back.

#### Scenario: A merge at the most chunks
- **WHEN** a grouping that reads a partition back has 32768 chunks
  when its merge needs another
- **THEN** the query fails with SQLSTATE 54000, and nothing past the
  directory is written
- **Verified by:** review only — 32768 chunks take hundreds of MiB;
  every write of a chunk into a directory of `nodes/` follows a
  reserve that checks the limit, and the reserve has its own test

#### Scenario: The limit of a directory
- **WHEN** a table's directory is reserved for 32767, 32768 and 32769
  chunks
- **THEN** the first two succeed and the third fails with SQLSTATE
  54000
- **Verified by:**
  `test/sql/table.sql::spec: table-memory/The limit of a directory`
