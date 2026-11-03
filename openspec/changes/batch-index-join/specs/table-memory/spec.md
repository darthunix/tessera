## MODIFIED Requirements

### Requirement: A serial build keeps its index within the limit
A `TessHashJoin` that builds its table in one process SHALL start to
spill before a batch when the chunks and values it has, room for two
more chunks, and the index for its records and the rows the batch
selects would pass `hash_mem`. A build SHALL stay in memory while all
of these fit. A join that builds a table for each outer batch from an
index lookup of the batch's keys SHALL never spill such a table: it
SHALL build it whole and count the memory past `hash_mem` as its
overrun.

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

#### Scenario: A last batch that doubles the index
- **WHEN** a serial join builds 262 150 rows of one integer key at
  `hash_mem` = 13 MiB, its last batch taking it past 262 144 records,
  where the index's buckets double
- **THEN** it spills, and its rows are the core's
- **Verified by:**
  `test/sql/join.sql::spec: table-memory/A last batch that doubles the index`

#### Scenario: Rows a filter rejects
- **WHEN** the build above selects 262 144 of its rows, its filter
  rejecting six rows of its last batches
- **THEN** it counts the rows its batches select and stays in memory
- **Verified by:**
  `test/sql/join.sql::spec: table-memory/Rows a filter rejects`

#### Scenario: A batch's table past the limit
- **WHEN** a join that looks a batch's keys up in an index finds more
  rows for them than the planner expected, past `hash_mem`
- **THEN** it builds the batch's table in memory, its `Overrun` is
  above 0, and its rows are the core's
- **Verified by:**
  `test/sql/index_join.sql::spec: table-memory/A batch's table past the limit`
