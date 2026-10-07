## ADDED Requirements

### Requirement: A key filter and its call
A parent node SHALL hand a key filter to its child as a `TessKeyFilter`
of `tessera/node.h`, through `tess_input_set_key_filter` and the
child's kind's optional `set_key_filter` callback. A key filter is the
keys of the parent's rows, as columns of the child's batches with their
kinds, `TESS_TABLE_KEY_INT4` or `TESS_TABLE_KEY_INT8`, and a Bloom
filter of their hashes: the words that `tess_bloom_probe` reads or,
when `shared`, a shared filter, which the child uses only once
`tess_bloom_shared_ready` says it is ready.

```
 TessKeyFilter
 ┌─────────────┬───────┬─────────┬────────┬────────┬────────┬────────┐
 │ struct_size │ nkeys │ columns │ kinds  │ words  │ nwords │ shared │
 └─────────────┴───────┴─────────┴────────┴────────┴────────┴────────┘
   valid for the call only:  the struct, columns[nkeys], kinds[nkeys]
   valid until taken back:   words[nwords], the parent's
```

The struct and the arrays of columns and kinds SHALL be valid only
during the call: a child that takes the filter SHALL copy what it keeps
of them. The words SHALL stay valid until the parent takes the filter
back with a call of `NULL`. The call SHALL return whether the child
took the filter; a child that refuses it SHALL keep what it had before
the call, and `NULL` is always taken. A child that is not a batch node
of Tessera, or whose kind has no callback, SHALL refuse. A filter whose
`struct_size` is below `TESS_KEY_FILTER_MIN_SIZE` SHALL raise an error,
SQLSTATE `XX000`.

#### Scenario: The call reaches the child's kind
- **WHEN** a filter of one key and one of two keys are handed to a kind
  that takes one key only, the filter is taken back, and a filter is
  handed to a child of no such kind
- **THEN** the first is taken, the second refused with the first still
  held, `NULL` taken, and the last refused
- **Verified by:** pending

#### Scenario: A filter from a parent's stack
- **WHEN** an append hands each partition below it the filter with the
  columns mapped to the partition's own, from an array of its stack
- **THEN** each partition removes the rows the filter rejects, and the
  join's rows are those of the core's plan
- **Verified by:** pending

#### Scenario: A filter too small for its fields
- **WHEN** a filter whose `struct_size` is below
  `TESS_KEY_FILTER_MIN_SIZE` is handed down
- **THEN** the call raises an error, SQLSTATE `XX000`
- **Verified by:** pending

### Requirement: Who hands a filter down
A hash join SHALL hand its Bloom filter to its outer child once it has
built the filter, when a row of the outer side without a pair leaves
the join's output: INNER, SEMI and RIGHT joins, not ANTI, LEFT or
FULL, which return such a row. It SHALL not hand it down when its table
spills, since the filter knows only the rows in memory, or when a key
is hashed from a value the child does not hold as a word. Once the
child took the filter, the join SHALL check its rows against it no
more. The join SHALL take the filter back before the table it was built
from goes: before it builds a new table, before a shared table is
freed, and before a participant leaves a shared table. A rescan that
keeps the table SHALL keep the filter below.

#### Scenario: The joins that hand a filter down
- **WHEN** INNER, SEMI, RIGHT, ANTI and LEFT joins over an outer side
  with a row-wise clause build a filter, alone and with a shared table
- **THEN** INNER, SEMI and RIGHT hand it down and show
  `Bloom Filter Below`, ANTI and LEFT check their rows themselves, and
  every join returns the rows of the core's plan
- **Verified by:** pending

#### Scenario: The joins that keep their filter
- **WHEN** a join's table spills, its key is hashed from a value, or
  its table is rebuilt for each value of a parameter
- **THEN** the first two never hand the filter down, the last takes it
  back before each new table, and the rows are those of the core's plan
- **Verified by:** pending

### Requirement: Who takes a filter
TessFilter SHALL take a filter when it has a clause that runs row by row,
which the filter saves the cost of, when the filter has 1 to 16 keys,
each a column of its child, not one it computes, of kind
`TESS_TABLE_KEY_INT4` or `TESS_TABLE_KEY_INT8`, and when the kernels
are loaded; otherwise it SHALL refuse it. TessAppend SHALL hand the
filter to each of its children with the keys mapped to that child's
columns, and SHALL take it only when every child takes it; when one
refuses, the children that took it SHALL give it back. No other node of
Tessera takes a filter.

#### Scenario: TessFilter takes a filter or refuses it
- **WHEN** a join's outer side is a TessFilter with a row-wise clause,
  one with only clauses that run in batches, and a partitioned table
  under TessAppend whose partitions' columns lie in another order
- **THEN** the first and every partition take the filter, the second
  refuses it and the join checks its rows itself
- **Verified by:** pending

### Requirement: What a child removes
A TessFilter that holds a filter SHALL, for each batch, before its first
clause that runs row by row and after the clauses that run in batches
before it, hash each remaining row's keys as the join hashes them,
remove the rows with a NULL key and the rows whose hash the filter
rejects, and pass the others on. A row it removes SHALL reach none of
its later clauses. A shared filter SHALL be checked only once the child
has seen it ready; until then every row SHALL pass. The rows the join
returns SHALL be those it would return without the filter below.

#### Scenario: The rows below a join
- **WHEN** INNER and SEMI joins hand their filter, alone or shared, to a
  TessFilter whose rows have NULL keys and keys without a pair
- **THEN** the joins return the rows of the core's plan
- **Verified by:** pending

### Requirement: The rows a filter removes, as EXPLAIN shows them
With `ANALYZE`, a hash join SHALL show `Rows Removed by Bloom Filter`:
the rows with a key that its filter rejected, its own filter's or that
of a table that spills, summed over its loops and over the participants
of a parallel plan, when it built a filter and it removed rows or none
went below. With `VERBOSE` it SHALL show `Bloom Filters`, the filters it
built, when there are any, and `Bloom Filter Below` when a child took
its filter. A TessFilter that held a filter SHALL show
`Rows Removed by Bloom Filter`, the rows with a key that the filter
rejected, and `Rows Removed by NULL Key`, the rows it removed for a NULL
key, each when it is above 0, per loop as its other lines and summed
over the participants. Both SHALL count in the core's
`Rows Removed by Filter` of the TessFilter, which counts every row the
node removed.

#### Scenario: The rows removed, line by line
- **WHEN** an INNER join hands its filter to a TessFilter whose rows
  have NULL keys, one row in 97, and keys without a pair
- **THEN** the TessFilter's `Rows Removed by Bloom Filter` counts rows
  with a key only, its `Rows Removed by NULL Key` the rows with a NULL
  key it read once the filter was below, and with the lines of its
  clauses they make the core's `Rows Removed by Filter`
- **Verified by:** pending
