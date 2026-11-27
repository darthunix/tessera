## Why

`TessAgg` keeps a state for each aggregate of a query and computes each
aggregate's argument for every row. An aggregate of an expression under
constants, such as `sum(x + 1)`, `avg(price * 100)` or `max(-x)`, is a
function of the aggregates of the expression itself: `sum(x + c)` is
`sum(x) + c * count(x)`, `max(-x)` is `-min(x)`. Several of them over
one expression could share two or three states and compute no argument
at all.

ClickBench's Q30 sums `ResolutionWidth` and `ResolutionWidth + k` for
`k` from 1 to 89: 90 sums of one `smallint`. Past `TessAgg`'s cap of 64
aggregates, it runs in the core's aggregate, which computes 89
additions and 90 sums a row: 139 billion cycles over `hits.parquet`,
137 times pg_duckdb's, 16 per cent of Tessera's cycles over the 39
queries counted. DuckDB folds the sums.

The cap itself comes from the record of a group: one word of flags, a
bit for each aggregate that has a value, which the merge of groups read
back reads. A query of more than 64 aggregates that no folding reduces
still runs in the core's aggregate.

This change takes up the roadmap's entry `agg-sum-plus-constant`, wider
than its one case, and the finding of "Not placed" "`TessAgg` refuses
more than 64 aggregates".

## What Changes

The first stage, aggregates of an expression under constants:

- `TessAgg` recognizes an argument `a·e + b` over one expression `e` of
  the input of type `smallint`, `integer`, `bigint` or `numeric` with a
  declared precision, without a volatile function in it or in its
  `FILTER`: `+` and `-` with a constant known when the query is planned
  on either side, `*` by such a constant, unary minus, and the casts of
  an integer to a wider integer or to `numeric`. Where at least two
  aggregates of one `e`, with one `FILTER` and one `DISTINCT`, are on
  the list, and one of them is not of `e` itself, the node keeps one sum
  state of `e`, its 128-bit sum and its count, and the extremes where
  they are asked, and derives each aggregate from those:
  - `sum`, `avg` and `count`;
  - `min` and `max`, of `numeric` only with a positive `a`;
  - with `DISTINCT`: `sum`, `avg` and `count` of integers, `count` of
    `numeric`;
  - aggregates of a constant, `sum(1)` say, from the count of rows.
- Each folded aggregate's value, type, scale and `NULL` are the core's
  for groups of fewer than 2^32 rows: the node derives it exactly, in
  128-bit integers or in `numeric`, and the core's functions make the
  result, `numeric_div` for an average and the cast to `bigint` for an
  integer's sum. A form of `numeric` is folded only where the planner
  proves that no row and no sum of it can pass the format or round.
- An integer argument that fails for some row the aggregate reads fails
  the query with an error the core raises for one of those rows, before
  the node returns its first row: the node keeps the least and the
  greatest `e` of the rows each folded `e` reads, over the whole input,
  and evaluates the argument at both by the core's executor. Where the
  types and constants rule the failure out, as for `smallint +
  integer`, it keeps neither.
- `EXPLAIN VERBOSE` of `TessAgg` shows `Folded Aggregates`, the count of
  aggregates it computes from shared states rather than their own
  arguments.
- Partial and final aggregations of a parallel plan fold nothing in
  this stage.
- Not folded, computed as now: `real`, `double precision`, `numeric`
  without a declared precision, `date`, `time`, `timestamp`,
  `timestamptz`, `interval` and `money`; division and modulo as steps;
  parameters of a generic plan and stable functions as constants.

The second stage, the cap of 64 aggregates lifted:

- The record of a group keeps a word of flags for each 64 aggregates,
  and one for none, the flag of aggregate `i` in word `i / 64`.
  `tess_table_combine` merges records of any count of aggregates within
  the table's payload; a record of 64 aggregates or fewer is laid out as
  before.
- `TessAgg` takes any count of aggregates whose grouping keys, states'
  arguments and `FILTER` conditions are at most 1 664 columns and whose
  group's record fits 64 kB; `docs/limitations.md` loses the cap.

## Capabilities

### New Capabilities

- `aggregate`: the aggregates `TessAgg` folds, the errors of their
  arguments, what `EXPLAIN` shows of them, the arguments it does not
  fold, and the count of aggregates a node takes. The node's other
  promises stay described in `docs/nodes.md` until the capability holds
  them.

### Modified Capabilities

- `hash-table`: the requirement "Merging groups read back", whose
  record holds a word of flags for each 64 aggregates and any count of
  aggregates.

## Impact

- Code: `nodes/agg_planner.c` (the form of an argument, the shared
  states, the costs, the count of columns), `nodes/agg.c`,
  `nodes/agg_group.c`, `nodes/agg_node.h` (the outputs apart from the
  states, the derived values, the check of the extremes, the words of
  flags), `nodes/agg_generic.c`, `nodes/agg_spill.c`, the version of the
  node's plan data; `crates/tessera-capi/src/c/table.rs` and
  `crates/tessera-kernels/src/accumulate.rs` (the merge of records with
  several words of flags).
- Public headers: `include/tessera/table.h`, the comment of
  `TessTableCombine` and the count of aggregates it takes; no signature
  changes.
- Settings: none. `EXPLAIN VERBOSE` gains `Folded Aggregates`.
- SQL suites: `test/sql/agg.sql`, `test/sql/parallel.sql`,
  `test/sql/table.sql`; the Rust tests of the merge.
- Benchmarks: a family of `bench/pg`, `aggfold`.
- Contracts: none covers the parts touched; `relhash`'s keeps a
  record's payload as bytes it does not read. The pull requests say so
  under Architecture.
- Roadmap: the entry `agg-sum-plus-constant` and the finding of the cap
  leave it at the close; `docs/limitations.md` loses the cap; what the
  first stage leaves out, the dates and times, the parallel plans and
  `numeric` without a precision, goes to the roadmap.
