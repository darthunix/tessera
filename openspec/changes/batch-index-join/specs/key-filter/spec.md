## MODIFIED Requirements

### Requirement: Who hands a filter down
A hash join SHALL hand its Bloom filter to its outer child once it has
built the filter, when a row of the outer side without a pair leaves the
join's output: INNER, SEMI, RIGHT and RIGHT ANTI joins, not ANTI, LEFT
or FULL, which return such a row. A table of fewer than 4096 records
SHALL get a filter under the same share of rows found as a larger one,
but only to hand down: when the child refuses it, the join SHALL drop it
and probe without one. A join that builds a table for each outer batch
from an index lookup of the batch's keys SHALL build no filter: its
table knows one batch's keys. It SHALL not hand it down when its table
spills, since the filter knows only the rows in memory, or when a key is
hashed from a value the child does not hold as a word. Once the child
took the filter, the join SHALL check its rows against it no more. The
join SHALL take the filter back before the table it was built from goes:
before it builds a new table, before a shared table is freed, and before
a participant leaves a shared table. A rescan that keeps the table SHALL
keep the filter below.

#### Scenario: The joins that hand a filter down
- **WHEN** INNER, SEMI, RIGHT, RIGHT ANTI, ANTI and LEFT joins over an
  outer side with a row-wise clause build a filter, alone and with a
  shared table
- **THEN** INNER, SEMI, RIGHT and RIGHT ANTI hand it down and show
  `Bloom Filter Below`, ANTI and LEFT check their rows themselves, and
  every join returns the rows of the core's plan
- **Verified by:**
  `test/sql/join.sql::The filter below the join: the outer side`;
  `test/sql/join.sql::A RIGHT join drops an outer row without a pair`;
  `test/sql/join.sql::A RIGHT ANTI join drops an outer row without a pair`;
  `test/sql/join.sql::The shared filter below the join: each participant`

#### Scenario: A small table's filter
- **WHEN** joins of a table of fewer than 4096 records whose probe rows
  mostly find no record, alone and with a shared table, have an outer
  side that takes the filter, and one that refuses it
- **THEN** the first hand it down and show `Bloom Filter Below`, the
  second check no filter, and every join returns the rows of the core's
  plan
- **Verified by:**
  `test/sql/join.sql::A small table's filter: a table of fewer than 4096`;
  `test/sql/join.sql::A shared small table hands its filter to each`

#### Scenario: The joins that keep their filter
- **WHEN** a join's table spills, its key is hashed from a value, or
  its table is rebuilt for each value of a parameter
- **THEN** the first two never hand the filter down, the last takes it
  back before each new table, and the rows are those of the core's plan
- **Verified by:**
  `test/sql/join.sql::A table that spills keeps its filter`;
  `test/sql/join.sql::A key hashed from a text value`;
  `test/sql/join.sql::Each table built for a parameter takes its filter back`

#### Scenario: A table per outer batch
- **WHEN** a join that looks each outer batch's keys up in an index
  runs at `tessera.join_bloom_ratio` = 1, over a TessFilter that would
  take a filter
- **THEN** it builds no filter, hands none down, and its rows are the
  core's
- **Verified by:** pending
