## MODIFIED Requirements

### Requirement: Who hands a filter down
A hash join SHALL hand its Bloom filter to its outer child once it has
built the filter, when a row of the outer side without a pair leaves
the join's output: INNER, SEMI, RIGHT and RIGHT ANTI joins, not ANTI,
LEFT or FULL, which return such a row. A table of fewer than 4096
records SHALL get a filter under the same share of rows found as a
larger one, but only to hand down: when the child refuses it, the join
SHALL drop it and probe without one. It SHALL not hand it down when
its table spills, since the filter knows only the rows in memory, or
when a key is hashed from a value the child does not hold as a word. Once the
child took the filter, the join SHALL check its rows against it no
more. The join SHALL take the filter back before the table it was built
from goes: before it builds a new table, before a shared table is
freed, and before a participant leaves a shared table. A rescan that
keeps the table SHALL keep the filter below.

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

### Requirement: Who takes a filter
TessFilter SHALL take a filter when it has a clause that runs row by row,
or when all its clauses run in batches, whose work the filter saves,
when the filter has 1 to 16 keys,
each a column of its child, not one it computes, of kind
`TESS_TABLE_KEY_INT4` or `TESS_TABLE_KEY_INT8`, and when the kernels
are loaded; otherwise it SHALL refuse it. TessAppend SHALL hand the
filter to each of its children with the keys mapped to that child's
columns, and SHALL take it only when every child takes it; when one
refuses, the children that took it SHALL give it back. No other node of
Tessera takes a filter.

#### Scenario: TessFilter takes a filter or refuses it
- **WHEN** a join's outer side is a TessFilter with a row-wise clause,
  one with only clauses that run in batches, one whose key it computes,
  and a partitioned table under TessAppend whose partitions' columns lie
  in another order
- **THEN** the first two and every partition take the filter, the third
  refuses it and the join checks its rows itself
- **Verified by:**
  `test/sql/join.sql::Only row-wise clauses: TessFilter takes them`;
  `test/sql/join.sql::Batch clauses after the filter: a TessFilter`;
  `test/sql/join.sql::a key the child`;
  `test/sql/join.sql::A partitioned outer side: TessAppend hands the filter`

### Requirement: What a child removes
A TessFilter that holds a filter SHALL, for each batch, check it before
its first clause that runs row by row and after the clauses that run in
batches before it, or, when all its clauses run in batches, before its
first clause, reading only the key columns. It SHALL hash each
remaining row's keys as the join hashes them,
remove the rows with a NULL key and the rows whose hash the filter
rejects, and pass the others on. A row it removes SHALL reach none of
its later clauses. A shared filter SHALL be checked only once the child
has seen it ready; until then every row SHALL pass. The rows the join
returns SHALL be those it would return without the filter below.

#### Scenario: The rows below a join
- **WHEN** INNER and SEMI joins hand their filter, alone or shared, to a
  TessFilter whose rows have NULL keys and keys without a pair
- **THEN** the joins return the rows of the core's plan
- **Verified by:**
  `test/sql/join.sql::The filter below the join: the outer side`;
  `test/sql/join.sql::The shared filter below the join: each participant`

#### Scenario: Batch clauses after the filter
- **WHEN** a join hands its filter to a TessFilter of batch clauses
  alone, on a column after the key
- **THEN** the TessFilter's `Rows Removed by Bloom Filter` counts rows
  its clauses never saw, its `Rows Removed by Batch Filter` only rows
  the filter passed, and the join returns the rows of the core's plan
- **Verified by:**
  `test/sql/join.sql::Batch clauses after the filter: a TessFilter`
