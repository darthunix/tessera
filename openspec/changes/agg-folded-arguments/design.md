# Design

## Context

`TessAgg` computes a plain aggregation or a grouping over batches. Its
planner collects the query's distinct aggregates into the node's scan
tuple, after the grouping expressions; expressions above the aggregates
are left to the plan's own projection. The plan's private data carries
one argument and one `FILTER` condition for each aggregate of the scan
tuple, and the node keeps one state for each, `values[i]` for the
aggregate in the scan tuple's place `i`, with a computed column of the
projection for each argument. A state is of one of these kinds:

- a count, a sum of `integer` and the extremes of the types a word
  holds, folded by the kernels into a word of the group's record;
- a sum state, of `sum` and `avg` of `numeric` and `bigint`, of `avg`
  of `integer` and `smallint`, and of `sum` of `smallint`: a 128-bit sum
  below 10^36 at the largest scale met, the count of the finite values,
  the scale and the flags of `NaN` and the infinities, and a `numeric`
  rest for what passes 10^36 (`include/tessera/table.h`);
- a generic state, which the core's transition function advances row by
  row over a batch.

A query of 90 sums of one column computes 90 arguments and keeps 90
states. The planner prices the node by the core's costs of every
aggregate of the query, `get_agg_clause_costs`, which reads the query's
own aggregates, and refuses more than 64 aggregates before it prices a
path or estimates a DISTINCT's pairs.

A group's record in the table is its keys, then a payload: one word of
flags, a bit for each state that has a value, then the states' words. A
sum of no rows is `NULL`, not 0, so the merge of groups read back after
a spill, `tess_table_combine`, reads the flags. That word caps the node
at 64 states.

## Goals / Non-Goals

**Goals:**

- At least two aggregates of one expression, one under constants, share
  one state of that expression, and the node computes neither their
  arguments nor states of their own.
- The core's values, types, scales, `NULL`s and errors, for groups of
  fewer than 2^32 rows.
- No count of aggregates past which the node refuses a query, but the
  columns it computes and the size of a group's record.

**Non-Goals:**

- Dates and times. `timestamp` plus a month clamps the day and keeps
  the time, so Jan 30 23:00 and Jan 31 01:00 come out in the other
  order; `timestamptz` plus a day goes through the local time, which a
  change of the clock reorders; and an infinite date stays infinite
  where the last finite one fails, so the least and the greatest value
  do not tell whether a row fails.
- `numeric` without a declared precision: its sum may pass the format's
  range where the sums of the forms do not, two rows of `6e131071` minus
  that constant say.
- Floating-point numbers, which round; `money`; `interval`.
- Parameters of a generic plan and stable functions as constants: their
  value, sign or `NULL` is known only at run time.
- The partial and final aggregations of a parallel plan: their partial
  states of `bigint` and `numeric` are the core's serialized ones or the
  node's own, and building them from shared states is work of its own.
- Groups of 2^32 rows or more: the core's 64-bit sum of `integer` values
  wraps on 18 and 19 and fails on master where a prefix passes its
  range, and the node's sum states, folded or not, keep 128 bits.
- Products of two expressions of the input, which no state of one of
  them gives.

## Decisions

The aggregates of ClickBench's Q30, before and after:

```
 SELECT sum(w), sum(w + 1), ..., sum(w + 89)        w a smallint

 now:   90 arguments a batch      w, w + 1, ..., w + 89
        90 states a group         sum, sum, ..., sum

 after: 1 argument a batch        w
        2 states a group          sum(w), count(w): S and n
        90 outputs at the end     S + k * n, k = 0 to 89
```

### The outputs apart from the states

The scan tuple stays the query's aggregates, which the plan's targets
and `HAVING` refer to. The plan's private data carries the states the
node keeps, each an aggregate with its argument and `FILTER` as now,
and for each aggregate of the scan tuple an output: a state's index, or
a derivation from states. The node keeps `values[]` for the states,
computes the outputs of a group when it emits it, into the scan slot or
the groups' batch, and resets both with the node. Arguments that are
equal expressions with no volatile function, in them or in their
`FILTER`, share one computed column; `sum(nextval('s'))` and
`max(nextval('s'))` keep two, as the core evaluates them twice. Without
folding every
output is its state, and the node works as before. This comes first,
in a commit that changes no behavior, with the plan data's version
raised.

### The folded form of an argument

The planner reads an argument as a folded form when it is `e` under
steps it knows, each the core's own function, recognized by its
identity: `int2pl`, `int4pl`, `int8pl` and the mixed ones (`int24pl`,
`int48pl`, `int84pl`, ...), the same for `-` and `*`, `int2um`,
`int4um`, `int8um`, `numeric_add`, `numeric_sub`, `numeric_mul`,
`numeric_uminus`, and the casts `int4(int2)`, `int8(int2)`,
`int8(int4)` and `numeric` of an integer. Each step's other side is a
non-`NULL` `Const` once the planner has folded constants, which a
parameter of a custom plan and an immutable function of literals are,
and a parameter of a generic plan and a stable function are not. `e` is
where the steps stop: any expression the node computes now, a column,
`x / 2` or `x + y`, with no volatile function in it or in the `FILTER`;
a cast to a `numeric` precision, a narrowing cast and a cast from
`numeric` end the steps and stay in `e`. `e` is of an integer type, or
a column of a table of `numeric` with a typmod: a table enforces the
typmod on every value it stores, which a function in `FROM` with a
column list does not, so such a value is finite or `NaN`, with at most
`precision - scale` digits before the point and `max(scale, 0)` after.
A `numeric` constant is finite; `NaN` and the infinities end the steps.
The form records `a` and `b` and its steps: integers while the form's
type is an integer, `numeric` from the step that makes it `numeric`,
`x::numeric * 1.5 + 0.25` say, built by the core's `numeric` functions
and kept as they come, not normalized, so that their scales stay.

- **Why identities:** an operator of an extension or a user's function
  is not a step, and its argument is left as it is.
- **Refused: a rewrite of the query before planning,** each aggregate
  replaced by an expression over others. The core's path and every
  other node would see the rewritten query.

### The forms of `numeric` proved safe

A form of `numeric` fails or rounds only past the format: more than
131 072 digits before the point, or a product of more than 16 383
digits of scale, `NUMERIC_DSCALE_MAX`, which `numeric_mul` rounds to.
The planner bounds every step of the form, and the derived `a * S + b *
n` for up to 2^63 rows, from `e`'s typmod, or an integer `e`'s type, 5,
10 or 19 digits at scale 0, and the constants' digits, and
folds the form only where every bound stays within a margin of the
format: the digits before the point within 100 000, the scale within
1 000. No row of such a form fails, no derived value fails or rounds,
and `NaN` passes through each step as through the core's. A form of
`numeric` thus needs no check of errors.

### The state shared by the aggregates of one expression

The planner groups the aggregates of the scan tuple by `e`, `FILTER`
and `DISTINCT`, before it checks the count of states, estimates the
pairs of DISTINCT and prices the path. A group of at least two
aggregates it can fold, one of them of a form other than `e` itself,
that would share no more states than it has aggregates, is folded:
`sum(x + 1), max(x)` would keep three, `sum(x)`, `count(x)` and
`max(x)`, and keeps its two. Any other aggregate keeps its own state. A
folded group keeps:

- for a `smallint` or an `integer` `e`, `sum(e)` and `count(e)`, words of
  the group's record: below 2^32 rows the 64-bit sum of `e` holds every
  sum the forms' own sums hold, `sum(x - 2147483647)` of many rows of
  2147483647 among them, and the groups spill as groups, as now;
- for a `bigint` `e`, the sum state of `avg(e)`, its sum in 128 bits and
  its `numeric` rest and its count, as `sum` of `bigint` keeps now;
- for a `numeric` `e`, the sum state of `avg(e)` and `count(e)`: the sum
  state counts the finite values alone and keeps `NaN` as a flag, so the
  count of the rows comes from `count(e)`;
- `min(e)` and `max(e)`, where it has extremes.

The sum states need 128-bit integers and the kernels module, as they do
now: without them the planner folds only integer forms of `smallint`
and `integer`.

The states are aggregates the planner makes, with the fields the core's
preprocessing of aggregates fills, the group's `FILTER` and its
`DISTINCT`; the node computes them as any. `sum(x)`, `avg(x)` and
`count(x)` with no other form are not folded: each is already a state
of `x`.

### The values derived

When the node emits a group, it derives each output from the group's
states, `n` the count of the rows and `S` the sum, `NaN` where the sum
state met one. For a form whose type is an integer, in 128-bit integers
with each step checked, and in `numeric` where a step passes 128 bits;
for a form whose type is `numeric`, in `numeric`:

- `sum` of a form of `smallint` or `integer`, a `bigint`: `a * S + b *
  n` cast to `bigint`, which fails with the core's `22003` "bigint out
  of range"; `NULL` where `n` is 0;
- `sum` of a form of `bigint` or `numeric`, a `numeric`: `a * S + b *
  n`, whose scale, the greater of `a`'s plus `S`'s and `b`'s, is the
  greatest of the forms' scales, as the core's sum's; `NULL` where `n`
  is 0;
- `avg`: `numeric_div` of that sum and `n`, as `int8_avg`, the average
  of `bigint` and `numeric_avg` divide the sum by the count; the sum of
  a form of `smallint` or `integer` goes through the cast to `bigint`
  first, as the core's average keeps it in a `bigint`; `NULL` where `n`
  is 0; `NaN` where `S` is, a group of `NaN` alone among them;
- `count`: `n`;
- `min`, `max`: the form's value at `min(e)`, or at `max(e)` where the
  factor of `e` is negative, `a * v + b` in the form's type, `NULL`
  without one. The check of errors keeps every step of the form within
  its type at the extremes of `e` over the input, which hold a group's
  own, so the value is the one the core's steps compute; the node's
  executor evaluates no expression a group.

A nested form, `((x + 2^30) * 2^30) * 2^30`, has `a` = 2^60 and `b` =
2^90, which 128-bit `a` and `b` hold; a form whose `a` or `b` passes 128
bits is not folded.

### The errors of an argument

The core fails at the first row whose argument fails: `x + c` past the
range of `integer`. Every integer step is monotone in `e` and fails only
past its type's range, and an integer has no infinity nor `NaN`, so the
values of `e` whose argument succeeds form an interval, and an argument
that fails for some row fails at the least or at the greatest `e` among
the rows read. For each folded integer `e` and `FILTER` whose form can
fail, the node keeps the least and the greatest `e` of the rows its
aggregates read over the whole input, not per group:

- taken from each batch over the rows its aggregates read, after the
  `FILTER`: in the frozen table's branch of a row spill, which takes the
  rows of groups it lacks aside before it computes the `FILTER` over the
  rest, the `FILTER` over the rows taken aside too, from the column the
  spill computes for them; the rows read back from a partition are not
  taken again;
- checked once the input ends, before the node reads a partition or a
  DISTINCT set back and before it returns a row: the original argument,
  every step in its own type, evaluated at both by the core's executor;
  the error that evaluation raises is the query's;
- kept in the node's own memory, which no reset of a partition's
  context frees, and cleared with the check's mark at a rescan.

`HAVING` and a `LIMIT` above come after the check.

- **When the types rule it out:** the planner computes each step's
  range from `e`'s type and the constants, both bounds, and keeps no
  extremes where every step stays within its type: `w + k` of a
  `smallint` `w` and an `integer` constant `k` from -2^31 + 2^15 to
  2^31 - 2^15 cannot fail.
- **Refused: extremes in every group,** which cost two states a group
  where one pair a node gives the same answer; **an error at the row,**
  which only computing the argument for every row gives.

### Aggregates of a constant

`sum(c)`, `avg(c)`, `min(c)`, `max(c)` and `count(c)` of a constant read
no column: in a plain aggregation or a grouping, not a partial or final
one, they are derived from `count(*)` with the aggregate's `FILTER`.
Where `n` is 0, `sum`, `avg`, `min` and `max` are `NULL` and `count` 0;
otherwise `sum` is `c * n`, a `bigint` for an integer of at most 32 bits
and a `numeric` for a `bigint` or a `numeric`, `avg` `numeric_div(c * n,
n)`, `min` and `max` `c`, `count` `n`. With `DISTINCT` they keep their
own states.

### DISTINCT

A form whose factor is not 0 maps the distinct values of an integer `e`
one to one, and those of a `numeric` `e` with a typmod by their value,
`NaN` to `NaN`, so `sum(DISTINCT a·e + b)` is `a * sum(DISTINCT e) + b *
count(DISTINCT e)` and `count(DISTINCT a·e + b)` is `count(DISTINCT e)`.
For an integer `e` the state is the sum state of `avg(DISTINCT e)`, one
set of pairs for all, which gives both; for a `numeric` `e`,
`count(DISTINCT e)`, which counts `NaN` once, as the core's. A folded
DISTINCT aggregate keeps no set of its own. Not `sum(DISTINCT)` of
`numeric`, whose scale depends on which of equal values comes first, nor
`min` or `max` of `numeric` with `DISTINCT`, which the node leaves to
the core now. The check of errors reads the extremes of every row, as
the core computes the argument of every row before it drops duplicates.

### EXPLAIN and the costs

`Folded Aggregates` counts the outputs derived from a state kept for
several aggregates; `agg_explain` shows it with the properties it shows
without `ANALYZE`. The planner prices a folded node by its own sum of
the states' arguments and transitions, from the costs of their
functions, and a derivation of each output for each group, without
changing the core's own costs of the query's aggregates.

### The words of flags (the second stage)

The payload keeps `max(1, (n + 63) / 64)` words of flags for `n`
states, as the spec of `hash-table` draws it. What reads or sizes them:

- `tess_table_combine`: its check of the count, its decoded kinds on the
  stack (`[Combine::Count; 64]` in `tessera-capi`), the flags' offsets
  and shifts in `accumulate.rs`;
- the node: `AGG_MAX_GROUPED` and the checks of the planner and of the
  node's plan; the first payload slot after the flags; the accumulate
  calls' flag offset and bit; the read and the gather of a group's
  flags (`flag_words`); the generic states' `payload[0]` bit; the merge
  of a partition read back in `agg_spill.c`; the planner's estimates of
  a group's bytes; `PairsEstimate.set_pairs`, sized by 64 and written
  for each DISTINCT aggregate.

The masks of 64 rows of a batch stay as they are. Two bounds take the
cap's place. The node's computed columns, its keys, its states'
arguments and their `FILTER` conditions, are at most 1 664, PostgreSQL's
bound of a tuple's columns: the spill of an aggregation's rows takes
4 096 words a row, so the bound is a choice that keeps a margin. And a
group's record fits the table's first chunk, 64 kB less its header, as
a record larger than a chunk would make the table add a chunk at every
try.

- **Refused: nodes of 64 aggregates each over one input,** which reads
  the input twice or copies it to each.

## Measurement plan

On the M5 Pro, PostgreSQL 19 with `pg_pmu`, cycles deciding,
instructions beside, every reading kept. The base of the first stage is
the commit of 2.1, which adds the cases; the base of the second is the
first stage's end.

- `bench/pg`, a new family `aggfold` over a table of 2 million rows of a
  `smallint`, an `integer`, a `bigint` and a `numeric(12,2)`, with
  groups of 37 and of 1 million: `cargo ab measure --counters --cycles
  --rounds 2 --deadline 300`, then without `--cycles` for the
  instructions, the cases split into runs of five minutes at most by
  `--cases`. Cases:
  - two forms of one column, the least a fold takes, and ten, of each
    type, without groups, with 37 and with a million;
  - ten forms of an expression dearer than a column, `abs(x) + k`;
  - the same with a `work_mem` that spills the groups, with the bytes
    written beside;
  - `DISTINCT` folded, with groups that spill their pairs;
  - folded and unfolded aggregates together;
  - guards that fold nothing: `sum(x + 1)` alone, `sum(x), avg(x),
    count(x)`, a `double precision` form, and a parallel plan forced by
    `parallel_setup_cost` and `parallel_tuple_cost` of 0.
  Each folded case has a twin over the same input that keeps the same
  states and derives nothing, `sum(x), count(x)` for sums of `x + k`,
  and its plan's states are read in `plans.txt`. The twin of the ten
  numeric sums of a million groups computes the same ten values from
  the same states above the aggregates, `sum(n) + k * count(n)`, as
  their reading adds ten numerics a group.
- ClickBench over `hits.parquet`, `cargo clickbench run --mode on
  --counters --tries 5 --workers 0`, one query a run, the base and the
  end in turns, twice, pg_duckdb's cycles beside: Q30, whose run at the
  base takes some 35 s a try, about 4 minutes with its warming and its
  plan, and Q3 and Q4 as guards; Q1 with Tessera off the control. Then
  the times of Q30 and of the spilled cases, `cargo clickbench run`
  and `cargo ab measure` without counters.
- The second stage: cases of 65 and 200 aggregates the node cannot fold,
  `sum(x / k)`, grouped and not, spilled, by `cargo ab measure` without
  counters, since their plan moves from the core's aggregate to
  `TessAgg` and counts compare no plans; the cases of 64 or fewer,
  counted. Before it, the machine code of the merge's loop at the base
  and the end.

Before the runs: the agent asks for an idle machine and records the
power source; release builds are installed and checked; plans are
compared.

Hypotheses of the first stage, written before the runs:

1. Q30 leaves the core's aggregate for `TessAgg` with two states: at
   least 90 per cent fewer cycles and less time.
2. The folded cases: within 10 per cent of their twins' cycles at the
   end, and fewer than at the base: two forms 0 to 30 per cent fewer,
   ten forms 50 to 85.
3. The cases with a million groups: fewer cycles than at the base, and
   within 20 per cent of their twins'.
4. The spilled cases: no more time and no more bytes written than at the
   base.
5. The guards: within 2 per cent; Q3, Q4 and the control within 2.

Hypotheses of the second stage:

6. The cases of 65 and 200 aggregates: at most half the time of the base,
   the core's.
7. The cases of 64 or fewer of both stages, Q3 and Q4: within 2 per cent
   of the first stage's end.

How a case is judged: a side's spread is the difference of its two
turns; a change smaller than the larger spread of the two sides, or
than the control's move, is no change; a side past 2 per cent is
counted once more, a third pair the plan sets now, and still past it
the case is unsettled, which the maintainer decides. A case of times
is judged as one of cycles, by its medians. A stage is kept when
hypotheses 2 to 4, or 6 and 7, hold and no case takes more than 2 per
cent more cycles or time than its base; Q30's gain is expected, not a
gate. A confirmed excess is profiled and searched as CONTRIBUTING.md
says before the maintainer is asked. The reports are kept under
`docs/benchmarks/`, with the readings, the plans and the bytes written.

## Risks / Trade-offs

- [An error at the end of the input, not at the row] → an error the core
  raises for one of the rows, SQLSTATE `22003`; a nested form whose
  steps fail in different rows may report another step's message than
  the core's first.
- [Groups of 2^32 rows or more] → the core's 64-bit sums of `integer`
  wrap or fail, the folded ones do not, as the node's sum states do
  already.
- [A typmod not enforced] → a function in `FROM` with a column list may
  give a `numeric(p,s)` column a value past its typmod, two of
  `6e131071` say; the planner folds `numeric` columns of tables alone,
  which enforce it, and a test shows such a function's column
  unfolded.
- [A step taken for monotone that is not] → the steps are a closed list
  of the core's integer and `numeric` functions; the dates and times
  are out of it, and tests show them unfolded.
- [A derived value of another scale or type] → the suites compare every
  type and step with the core, values and `pg_typeof`.
- [The cost of the derivation] → 128-bit integers for integers, one
  `numeric` division an average; the cases of a million groups gate it.
- [A record of more than 64 states] → its merge reads each flag in its
  word; tests merge 0, 63, 64, 65, 127, 128 and 130 states in every
  combination of flags, payloads exact and one word short, and a wrong
  kind past the 64th.

## Decisions of the maintainer

- The fold covers the types and aggregates this design lists, not the
  one case of Q30.
- `DISTINCT` is folded in the first stage.
- The cap of 64 aggregates is lifted as the second stage of this change.
- Cycles decide the counts; instructions help but come second.

## Files

- `nodes/agg_planner.c`: the form of an argument, the shared states, the
  outputs, the costs, the bounds.
- `nodes/agg.c`, `nodes/agg_group.c`, `nodes/agg_node.h`,
  `nodes/agg_generic.c`, `nodes/agg_spill.c`, `nodes/agg.h`: the outputs
  apart from the states, the shared computed columns, the derived
  values, the check of the extremes, the words of flags; the plan data's
  version.
- `include/tessera/table.h`, `crates/tessera-capi/src/c/table.rs`,
  `crates/tessera-kernels/src/accumulate.rs`: the merge's words of
  flags.
- `test/sql/agg.sql`, `test/sql/parallel.sql`, `test/sql/table.sql` and
  their expected output; `crates/tessera-kernels/src/accumulate.rs`,
  `crates/tessera-capi/tests/table.rs`.
- `bench/pg/aggfold.sql`, which makes its own table, and
  `bench/pg/README.md`.
- `docs/limitations.md`, `docs/nodes.md`; at the close
  `openspec/specs/aggregate/design.md` and
  `openspec/specs/hash-table/design.md`.

## Tests

- `test/sql/agg.sql`, against the same queries with Tessera off, with
  `pg_typeof` of each value and `EXPLAIN VERBOSE`'s `Folded Aggregates`
  so that a test shows the fold took place: every aggregate and type
  under each step, a constant on either side, negation, nested forms,
  `NULL` values and `NaN`, no rows, a `FILTER` that leaves a group no
  row, `GROUP BY`, groups past `hash_mem`; ninety sums of one column;
  the groups that are not folded, a volatile `FILTER` and a volatile
  argument among them; the forms not folded, a parameter under
  `plan_cache_mode = force_generic_plan`, an infinite constant and a
  `numeric(12,2)` column of a function in `FROM` among them; groups of
  `NaN` alone and mixed, in memory and spilled; the errors at each
  integer's least and greatest value, nested, a failing row left out by
  `WHERE` and by `FILTER`, kept past `HAVING`, a `LIMIT` and a spill,
  against the core's hashed aggregation, and a rescan with other rows;
  aggregates of a constant; `DISTINCT`, with a factor of 0 and pairs
  past `hash_mem`.
- `test/sql/parallel.sql`: the same aggregates in a parallel plan,
  folding nothing, against Tessera off.
- The second stage: the merge of 0, 63, 64, 65, 127, 128 and 130 states
  in every combination of flags, payloads exact and one word short, and
  a wrong kind past the 64th, in Rust and through the C API;
  `test/sql/agg.sql` and `test/sql/parallel.sql` with 65 and 200
  aggregates, grouped and not, generic and DISTINCT among them, past
  `hash_mem`; groupings past 1 664 columns and past 64 kB a group left
  to the core.
- `make crosscheck` after each stage.
