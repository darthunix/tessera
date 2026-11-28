# aggregate: how it is built

`TessAgg` is Tessera's node of a plain aggregation, without `GROUP
BY`, and of a grouping: it reads batches from its child and folds them
into a state for each aggregate of each group. This page covers two of
its parts: how it folds aggregates of one expression into states they
share, and how many aggregates one node takes. Its other parts, the
table of groups, the generic aggregates, the dictionaries of keys, the
`DISTINCT` sets and the spill, are described in
[docs/nodes.md](../../../docs/nodes.md).

This page explains the idea and how it is built. The exact rules, the
errors and what `EXPLAIN` shows are in the capability's spec,
[spec.md](spec.md), next to this page.

## Background

The node's planner collects the query's distinct aggregates into the
node's **scan tuple**, after the grouping expressions. The plan's
targets and `HAVING` refer to the scan tuple; expressions above the
aggregates stay in the plan's own projection. For each aggregate the
node computes its argument, and its `FILTER` condition where it has
one, as a **computed column** of the batch, and folds the column into
a **state**. A state is of one of three kinds:

- a **word state**: a count, a sum of `integer`, and the extremes of
  the types a 64-bit word holds, which the kernels fold a batch at a
  time into a word of the group's record;
- a **sum state**, of `sum` and `avg` of `numeric` and `bigint`, of
  `avg` of `integer` and `smallint`, and of `sum` of `smallint`: a
  128-bit sum below 10^36 at the largest scale met, the count of the
  finite values, the scale, flags of `NaN` and the infinities, and a
  `numeric` rest for what passes 10^36
  ([table.h](../../../include/tessera/table.h));
- a **generic state**, which the core's transition function advances
  row by row over a batch.

With `GROUP BY`, a group lives in the hash table as a **record**: its
keys, then a payload of **words of flags**, a bit for each state that
has a value, then the states' words. A sum of no rows is `NULL`, not 0,
so the merge of a group's records read back after a spill needs the
flags.

## The problem

A query often computes several aggregates of one expression shifted or
scaled by constants. ClickBench's Q30 sums a `smallint` column and that
column plus each integer from 1 to 89: 90 sums. A node that computes
each aggregate from its own argument computes 90 arguments a batch and
keeps 90 states a group, though the 90 sums follow from two numbers, the
sum of the column and the count of its values.

A record also held one word of flags, so a node took at most 64
aggregates, and a query of more ran in the core's aggregate, row by
row.

## Goals and what they cost

- **One state for the aggregates of one expression.** At least two
  aggregates of one expression, one of them under constants, share the
  states of that expression, and the node computes neither their
  arguments nor states of their own. It costs a derivation of each
  output when a group is emitted, and the planner's work to read the
  forms.
- **The core's answers.** Values, types, scales, `NULL`s and errors as
  the core gives them, for groups of fewer than 2^32 rows. An argument
  the node no longer computes can no longer fail at its row, so a
  folded argument that can fail costs a check of errors over the whole
  input, and the error comes at the end of the input.
- **No count of aggregates.** The words of flags grow with the
  aggregates. It costs the merge a few instructions an aggregate, to
  find each flag's word, and two bounds take the count's place: the
  columns the node computes and the size of a group's record.

## The whole in one picture

Q30's aggregates, computed apart and folded:

```
 SELECT sum(w), sum(w + 1), ..., sum(w + 89)        w a smallint

 apart:  90 arguments a batch      w, w + 1, ..., w + 89
         90 states a group         sum, sum, ..., sum

 folded: 1 argument a batch        w
         2 states a group          sum(w), count(w): S and n
         90 outputs at the end     S + k * n, k = 0 to 89
```

Where each piece works:

```
 planning       each aggregate's argument read as a form a·e + b
                aggregates of one e, FILTER and DISTINCT grouped
                the states of a group chosen; an output for each
                aggregate: a state, or a derivation from states
                the probes of the forms that can fail
                        │ plan data: states, outputs, checks
                        ▼
 each batch     the states' arguments computed and folded
                the least and the greatest e of the rows read
                        │
 input's end    the probes evaluated at both extremes:
                an error there is the query's
                        │
 each group     the outputs derived from the group's states
                into the scan tuple, which the targets read
```

## The outputs apart from the states

The scan tuple stays the query's aggregates. The plan data carries the
states the node keeps, each an aggregate with its argument and its
`FILTER`, and for each aggregate of the scan tuple an **output**: the
index of a state, or a derivation from states. The node keeps the
states, computes the outputs of a group when it emits it, into the
scan slot or the groups' batch, and resets both with the node. Without
a fold every output is its own state, and the node works as before.

Arguments that are equal expressions with no volatile function, in them
or in their `FILTER`, share one computed column: `sum(x), max(x)`
computes `x` once. `sum(nextval('s'))` and `max(nextval('s'))` keep two
columns, as the core evaluates them twice.

- **Refused: a rewrite of the query before planning,** each aggregate
  replaced by an expression over others. The core's paths and every
  other node would see the rewritten query, and the core's path would
  no longer be the query the user wrote.

## The form of an argument

The planner (`fold_form` in `nodes/agg_planner.c`) reads an argument
as a **folded form**: an expression `e` under steps it knows, each the
core's own function, recognized by its identity: the additions,
subtractions and products of `smallint`, `integer` and `bigint` and
their mixed versions, their unary minus, the casts of an integer to a
wider one or to `numeric`, and `numeric`'s addition, subtraction,
product and unary minus. Each step's other side is a non-`NULL`
constant once the planner has folded constants: a literal, an
immutable function of literals and a parameter of a custom plan are;
a parameter of a generic plan and a stable function are not, as their
value is known only when the query runs.

`e` is where the steps stop: any expression the node computes, a
column, `x / 2` or `x + y`, with no volatile function in it or in the
`FILTER`. A cast to a `numeric` precision, a narrowing cast and a cast
from `numeric` end the steps and stay in `e`. `e` is of an integer
type, or a column of a table of `numeric` with a declared precision. A
table enforces the precision on every value it stores; a function in
`FROM` with a column list does not, so only a table's column is sure to
hold a finite value or `NaN` with at most `precision - scale` digits
before the point.

A form records its factor `a` and its offset `b`, as integers while its
type is an integer, and in `numeric` from the step that makes it
`numeric`, as the core's `numeric` functions build them, so that their
scales stay. `a` and `b` of an integer form are 128-bit:
`((x + 2^30) * 2^30) * 2^30` has `a` = 2^60 and `b` = 2^90. A form
whose `a` or `b` passes 128 bits is not folded.

- **Why identities:** an operator of an extension or a user's function
  is not a step, so its argument is left as it is.

## The forms of `numeric` proved safe

A `numeric` value fails or rounds only past the format: more than
131 072 digits before the point, or a product of more than 16 383
digits of scale, which `numeric_mul` rounds to. The planner bounds
every step of a `numeric` form, and the derived `a * S + b * n` for up
to 2^63 rows, from the precision of `e`, or the digits of an integer
`e`'s type, and the constants' digits. It folds the form only where
every bound stays within a margin of the format: 100 000 digits before
the point and a scale of 1 000, checked before the arithmetic of the
bounds. No row of such a form fails, no derived value fails or rounds,
and `NaN` passes each step as it passes the core's. A form of
`numeric` needs no check of errors.

`numeric` without a precision has no such bound: two rows of
`6e131071` minus that constant succeed alone, and their sum fails in
the core where the sums of the forms would not. It is not folded.

## The states shared by the aggregates of one expression

The planner (`fold_aggregates`) groups the aggregates of the scan
tuple by `e`, `FILTER` and `DISTINCT`, before it checks the bounds of
the node, estimates the pairs of `DISTINCT` and prices the path. A
group is folded where it has at least two aggregates it can fold, one
of them of a form other than `e` itself, and where the states they
would share are no more than they are (`fold_states`): `sum(x + 1),
max(x)` would keep three states, `sum(x)`, `count(x)` and `max(x)`, so
it keeps its own two. A folded group keeps:

- for a `smallint` or an `integer` `e`, `sum(e)` and `count(e)`, word
  states: below 2^32 rows the 64-bit sum of `e` holds every sum the
  forms' own sums would, `sum(x - 2147483647)` of many rows of
  2147483647 among them;
- for a `bigint` `e`, the sum state of `avg(e)`, its 128-bit sum, its
  `numeric` rest and its count;
- for a `numeric` `e`, the sum state of `avg(e)` and `count(e)`: the
  sum state counts the finite values alone and keeps `NaN` as a flag,
  so the count of the rows comes from `count(e)`;
- `min(e)` and `max(e)`, where it has extremes.

The sum states need 128-bit integers and the kernels module: without
them the planner folds only the integer forms of `smallint` and
`integer`. The states are aggregates the planner makes, with the fields
the core's preprocessing of aggregates fills, the group's `FILTER` and
its `DISTINCT`, so the node computes them as any other. `sum(x)`,
`avg(x)` and `count(x)` with no other form fold nothing: each is
already a state of `x`.

The partial and final aggregations of a parallel plan fold nothing:
their partial states of `bigint` and `numeric` are the core's
serialized ones or the node's own, and building them from shared states
is work of its own.

## The values derived

When the node emits a group (`agg_derive` in `nodes/agg.c`), it derives
each output from the group's states, `n` the count of the rows and `S`
the sum, `NaN` where the sum state met one. A form whose type is an
integer is derived in 128-bit integers, each step checked, and in
`numeric` where a step passes 128 bits; a form whose type is `numeric`
in `numeric`.

- `sum` of a form of `smallint` or `integer`, a `bigint`: `a * S + b *
  n` cast to `bigint`, which fails with the core's error, `22003`
  "bigint out of range"; `NULL` where `n` is 0.
- `sum` of a form of `bigint` or `numeric`, a `numeric`: `a * S + b *
  n`, whose scale, the greater of `a`'s plus `S`'s and `b`'s, is the
  greatest scale of the forms' values, as the core's sum's is; `NULL`
  where `n` is 0.
- `avg`: that sum divided by `n` as the core divides it, through the
  cast to `bigint` first for a form of `smallint` or `integer`, whose
  average the core keeps in a `bigint`; `NULL` where `n` is 0, `NaN`
  where `S` is.
- `count`: `n`.
- `min` and `max` (`agg_derive_extreme`): `a * v + b` in the form's
  type, `v` the least `e` of the group, or its greatest where `a` is
  negative; `NULL` without one. The check of errors below keeps every
  step of the form within its type at the extremes of `e` over the
  whole input, which hold each group's, so this is the value the core's
  steps compute, and the node evaluates no expression a group.

## The errors of an argument

The core fails at the first row whose argument fails: `x + c` past the
range of `integer`, say. A folded argument is not computed row by row,
so the node finds such a row another way. Every integer step is
monotone in `e` and fails only past its type's range, and an integer
has no infinity nor `NaN`. So the values of `e` whose argument succeeds
form an interval, and an argument that fails for some row fails at the
least or at the greatest `e` of the rows read.

For each folded integer `e` and `FILTER` whose form can fail, the node
keeps the least and the greatest `e` of the rows its aggregates read,
over the whole input, not per group:

- taken from each batch after the `FILTER`, in one pass over the
  batch's words of 64 rows (`agg_check_take`). Where the table is full
  and the node spills the rows of groups it lacks, it takes them from
  those rows too, before it sends them to disk; the rows read back from
  disk are not taken again;
- checked once the input ends, before the node reads a partition or a
  `DISTINCT` set back and before it returns a row (`agg_check_finish`):
  the original argument, every step in its own type, is evaluated at
  both extremes by the core's executor, its **probe**, and the error
  that evaluation raises is the query's;
- kept in the node's own memory, which no reset of a partition's
  memory frees, and cleared at a rescan, which checks the rows it reads
  alone.

`HAVING` and a `LIMIT` above the node come after the check, so they do
not keep an error from the query. A nested form whose steps fail in
different rows may report another step's message than the core's first
row would; the SQLSTATE is the same, `22003`.

Where the types rule an error out, the node keeps no extremes: the
planner computes each step's range from `e`'s type and the constants,
and `w + k` of a `smallint` `w` and an `integer` `k` between -2^31 +
2^15 and 2^31 - 2^15 cannot fail.

The pass over whole words costs some 1.6 cycles a row. Reading each
row through a call and two branches took some 9, and the kernels'
batch functions of `min` and `max`, two calls a batch, some 6.

- **Refused: extremes in every group,** which cost two states a group
  where one pair for the node gives the same answer.
- **Refused: an error at the row,** which only computing the argument
  for every row gives, the work the fold saves.

## Aggregates of a constant

`sum(c)`, `avg(c)`, `min(c)`, `max(c)` and `count(c)` of a constant
read no column. In a plain aggregation or a grouping, not a partial or
final one, they are derived from `count(*)` under the aggregate's
`FILTER`. Where `n` is 0, `sum`, `avg`, `min` and `max` are `NULL` and
`count` 0; otherwise `sum` is `c * n`, a `bigint` for an integer of at
most 32 bits and a `numeric` for a `bigint` or a `numeric`, `avg` that
sum divided by `n`, `min` and `max` `c`, and `count` `n`. With
`DISTINCT` they keep their own states. A constant's scale is checked
when the node reads its plan, so a plan of another build cannot give it
a scale the derivation does not hold.

## DISTINCT

A form whose factor is not 0 maps the distinct values of an integer `e`
one to one, and those of a `numeric` `e` with a precision by their
value, `NaN` to `NaN`. So `sum(DISTINCT a·e + b)` is `a *
sum(DISTINCT e) + b * count(DISTINCT e)`, and `count(DISTINCT a·e + b)`
is `count(DISTINCT e)`. For an integer `e` the state is the sum state
of `avg(DISTINCT e)`, one set of distinct values for all the group's
aggregates, which gives both; for a `numeric` `e`, `count(DISTINCT e)`,
which counts `NaN` once, as the core does.

Not folded: `sum(DISTINCT)` of `numeric`, whose scale depends on which
of equal values comes first, and `min` and `max` of `numeric` with
`DISTINCT`, which the node leaves to the core. The check of errors reads
the extremes of every row, as the core computes the argument of every
row before it drops duplicates.

## EXPLAIN and the costs

`EXPLAIN VERBOSE` shows `Folded Aggregates`, the count of the outputs
derived from states kept for several aggregates, with the properties
the node shows without `ANALYZE`. The planner prices a folded node by
its own sum of the states' arguments and transitions, from the costs of
their functions, and a derivation of each output for each group, and
leaves the core's costs of the query's aggregates as they are.

## The words of flags and the bounds of a node

A record keeps one word of flags for each 64 states, `max(1, (n + 63) /
64)` words for `n` states: the flag of state `i` is bit `i % 64` of word
`i / 64`, and the states follow the flags. The layout and the rules of
the merge are in the spec of the hash table,
[Merging groups read
back](../hash-table/spec.md#requirement-merging-groups-read-back). A
record of 64 states or fewer is the same as with one word. In the node
the flags of a batch's groups are read and gathered a word at a time;
the masks of 64 rows of a batch do not change.

Two bounds take the place of a count of aggregates. The planner checks
them (`states_fit`), and the node checks them again when it reads its
plan:

- The node's computed columns, its keys, its states' arguments and
  their `FILTER` conditions, are at most 1 664, PostgreSQL's bound of a
  tuple's columns. The spill of a grouping's rows holds 4 096 words a
  row, so the bound keeps a margin.
- A group's record fits the table's first chunk, 64 kB less the chunk's
  header: a record larger than a chunk would make the table add a chunk
  at every try. The planner counts each state's words from above, a sum
  state's and a numeric extreme's where the kernels fold them, so the
  node lays out no more than the planner counted.

Past either bound the grouping is the core's.

- **Refused: nodes of 64 aggregates each over one input,** which would
  read the input twice or copy it to each.

## What we decided not to do

- **Dates and times.** `timestamp` plus a month clamps the day and
  keeps the time, so Jan 30 23:00 and Jan 31 01:00 come out in the
  other order; `timestamptz` plus a day goes through the local time,
  which a change of the clock reorders; and an infinite date stays
  infinite where the last finite one fails. The least and the greatest
  value do not tell whether a row fails.
- **Floating-point numbers, `money` and `interval`.** Floats round, so
  a sum of shifted values is not the shifted sum.
- **Products of two expressions of the input,** which no state of one
  of them gives.
- **Groups of 2^32 rows or more.** The core's 64-bit sum of `integer`
  values wraps or fails there, and the node's sum states, folded or
  not, keep 128 bits.
- **A spill of rows for wide records.** The node spills a partition's
  records whole, and a partition that went to disk makes new records of
  its next rows. With many states and groups whose rows lie far apart,
  nearly every row becomes a record on disk: 65 sums in 100 000 groups
  of 20 rows past a `work_mem` of 4 MB write ten times the bytes the
  core's hashed aggregation writes of its input rows, and take 1.85
  times its time; 16 sums take 0.79 of it. The node's spill of rows
  writes the computed arguments, as wide as the record. A spill of the
  child's columns, the arguments computed again when the rows are read
  back, is work of its own.

## Files

- `nodes/agg_planner.c`: the form of an argument, the bounds of a
  numeric form, the groups and their states, the outputs and the
  probes, the costs of a folded path, and the bounds of a node
- `nodes/agg.c`: the plan data and its checks, the derived values, the
  check of errors and its extremes
- `nodes/agg_group.c`, `nodes/agg_spill.c`: the words of flags of the
  groups' records, read, gathered and merged
- `nodes/agg_generic.c`: the conversion of a 128-bit integer to
  numeric, and the sum and the count of a sum state
- `nodes/agg.h`, `nodes/agg_node.h`: the words of flags, the outputs
  and the node's state
- `crates/tessera-kernels/src/accumulate.rs`,
  `crates/tessera-capi/src/c/table.rs`, `include/tessera/table.h`: the
  merge of records over words of flags
- `bench/pg/aggfold.sql`: the family of benchmarks of the fold and of
  more than 64 aggregates

## Tests

- `test/sql/agg.sql`, the section of aggregates folded from shared
  states: every query against the same with Tessera off, with
  `pg_typeof` of each value and `EXPLAIN VERBOSE`'s `Folded Aggregates`
  so that a test shows the fold took place: every aggregate and type
  under each step, a constant on either side, negation, nested forms,
  `NULL` values and `NaN`, no rows, a `FILTER` that leaves a group no
  row, `GROUP BY`, groups past `hash_mem`; the groups and forms not
  folded; the errors at each integer's least and greatest value, nested,
  a failing row left out by `WHERE` and by `FILTER`, kept past `HAVING`,
  a `LIMIT` and a spill, against the core's hashed aggregation, and a
  rescan with other rows; aggregates of a constant; `DISTINCT`; 65 and
  200 aggregates, grouped and not, generic and `DISTINCT` among them,
  past `hash_mem`; groupings past 1 664 columns and past 64 kB a group.
- `test/sql/parallel.sql`: folded aggregates in a parallel plan,
  folding nothing, and 65 aggregates in a partial and a final
  `TessAgg`.
- `test/sql/coverage.sql`: 65 sums of different expressions in the
  node.
- `crates/tessera-kernels/src/accumulate.rs` and
  `crates/tessera-capi/tests/table.rs`: the merge of 0, 63, 64, 65,
  127, 128 and 130 states, flags set and clear in each word, payloads
  exact and one word short, and an unknown kind past the 64th.
