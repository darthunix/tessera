# Design

## Context

TessAgg takes an aggregate in one of two ways.

- **The node's own aggregates**: `count`, `sum` of `integer`, and `min`
  and `max` of `integer` and `bigint`. Without `GROUP BY`, a kernel of
  `tessera_kernels` computes a batch's partial value from the column,
  and the node joins it to the running value. With `GROUP BY`, the
  kernels of the hash table fold a batch's rows into each group's
  record, a word of 64 bits a value. The planner calls these
  aggregates the node's own (`batch_aggregate`) and costs them so.
- **The generic path**: any other aggregate the node accepts goes
  through the core's functions. Its transition function is called for
  each selected row, with a stand-in context. With `GROUP BY` each
  group keeps the core's state. A partial grouping of a parallel plan
  takes no generic aggregate but a sum state.

`min(date)` takes the generic path. Its transition function is
`date_smaller`, which returns the smaller of two integers. The same
holds for `max(date)` with `date_larger`, for `smallint`, `timestamp`,
`timestamptz`, `time` and `money` with their own pairs of functions,
and for `bool_and`, `bool_or` and `every`, whose functions are the
`and` and the `or` of two booleans.

The node holds some types as integers, a value in a word of 64 bits
with its sign extended, as PostgreSQL's `Datum` of the type is
(`tess_word_key_kind`): `smallint`, `integer`, `date` and `boolean` as
words of four bytes, `bigint`, `timestamp` and `timestamptz` as words
of eight. Their comparisons go to the kernels of `integer` and `bigint`
(`kernels/int.c`), and they are keys of the hash table and of the sort
as their words. A key of another type goes in as a number for the
value: in a grouping its number in a dictionary, in a join its hash,
by the type's own functions. A type without a hash function, as
`money`, gets no number, and a grouping by it stays the core's.

A batch's column is a column of `Datum`s whatever the source. The
Parquet source turns the file's days since 1970 into PostgreSQL's days
since 2000 before it gives them. So a kernel reads a column of dates as
it reads one of integers.

The core compares two values of these types as their integers, the
infinities too: a `-infinity` is the least integer of its width, an
`infinity` the greatest. Two equal values are the same integer, so
which of them an extreme keeps does not show. A `time` is an `int64`
of microseconds since midnight, a `money` an `int64` of cents; their
equality and order are an `int64`'s, and both are passed by value.

An aggregate whose answer is the first value in the order of a sort
operator names that operator in the catalog, `pg_aggregate.aggsortop`:
`<` for `min`, `bool_and` and `every`, `>` for `max` and `bool_or`.
PostgreSQL's planner takes the answer of such an aggregate from an
index, and ignores a `DISTINCT` in it (`planagg.c`).

## Goals / Non-Goals

**Goals:**

- Every extreme of a type the node holds as an integer by the kernels
  of the extremes that exist, on every path the extremes of `integer`
  take.
- `time` and `money` held as integers of eight bytes, as `bigint` is:
  their comparisons, keys and extremes.
- `DISTINCT` ignored in these extremes.
- No new kernel and no new code in Rust.

**Non-Goals:**

- A faster deform of the Parquet column `EventDate`, or of any source.
- Types the node does not hold as integers after the change: see
  "What we do not do".

## Decisions

The way of `min(d)` over a column of dates, before and after:

```
 Was:                               Will be:

 batch of 64 rows                   batch of 64 rows
   │                                  │
   ▼                                  ▼
 for each selected row:             tess_int4_min(column)
   date_smaller(state, value)         │ the batch's least value,
   through the core's call            │ one call a batch
   │                                  ▼
   ▼                                join the running value
 the state, a date                    as int4
```

With `GROUP BY` the same choice holds: the hash table's kernels fold
`TESS_TABLE_MIN_INT4` or `TESS_TABLE_MIN_INT8` into the group's word
in place of the core's state that the group kept.

### The extremes registered with the integers'

- **Was:** `tessera_kernels` registers `min` and `max` of `integer`
  with the codes `AGG_MIN_INT4` and `AGG_MAX_INT4`, and of `bigint`
  with `AGG_MIN_INT8` and `AGG_MAX_INT8`.
- **Will be:** it also registers, with the codes of `integer`, `min`
  and `max` of `smallint` and `date`, `bool_and` and `every` as minima
  and `bool_or` as a maximum; with the codes of `bigint`, `min` and
  `max` of `timestamp`, `timestamptz`, `time` and `money`. A kernel
  reads a `Datum`'s low 32 or all 64 bits, and gives back an
  `Int32GetDatum` or an `Int64GetDatum`: the same bits as the `Datum`
  of a `smallint`, a `date`, a `boolean`, a timestamp, a `time` or a
  `money` of that value.
- **Refused:** codes and kernels of their own for these types. They
  would repeat the loops of the integers line for line.
- **Refused:** a fold of the generic path, as the node keeps for `min`
  and `max` of `numeric` and `float8`. It still runs a row at a time,
  and with `GROUP BY` keeps a state a group outside the word, which a
  partial grouping does not take.

### The node's own aggregates in the planner

- **Was:** `aggregate_kind` names `min` and `max` of `integer` and
  `bigint`; `batch_aggregate` takes them when the argument's type is
  `integer` or `bigint`.
- **Will be:** `aggregate_kind` names the extremes above, a minimum or
  a maximum each, and `batch_aggregate` takes an argument of any type
  the node holds in a word, with `time` and `money`. The cost follows:
  the planner costs them as the node's own, a word of the group's
  record and no state of the core.

### The width of an extreme

- **Was:** an extreme is of eight bytes when the aggregate's transition
  type is `bigint` (`value->wide`), else of four.
- **Will be:** of eight bytes when the transition type is one a word
  of eight bytes holds, by the rule of the keys (`tess_word_key_kind`).
  The width picks the kernel's kind of state, reads a participant's
  partial value, and makes the result's `Datum`. Those three places
  stay as they are.

### `time` and `money` as integers

- **Was:** neither type is one the node holds in a word. A comparison
  of them runs row by row. A grouping, a `DISTINCT`, a `UNION` and a
  join by `time` take it as a number: its number in a dictionary or
  its hash, and a join keeps the equality as its filter. A grouping by
  `money` stays the core's, as the type has no hash function.
- **Will be:** `tess_word_key_kind` gives both the kind of eight bytes.
  The comparisons of both register with those of `bigint`, and the
  lists of `IN` and `NOT IN` of both take the words' sets
  (`runtime/expr.c`). The equality of `time` is one of words for a
  join (`word_equality`). A sort's order of them is their words', as
  their default B-tree families order them.
- A grouping by `money` becomes the node's: with no hashed grouping of
  the core to stand beside, the node hashes in place of the core's
  sorted one, as it does when the core's sorted grouping wins.
- A join by `money` stays the core's. PostgreSQL does not mark the
  equality of `money` as one a hash join can use, so the core makes no
  hash join for it, and the node's join takes the core's hashed
  clauses only. A `DISTINCT` and a `UNION` of `money` stay the core's
  too: the node offers its plan beside the core's hashed one alone.
- **Refused:** keeping `time` in the second form for its keys and
  adding the words only for the extremes. The node would then hold a
  type in two forms, which the one list of `tess_word_key_kind`
  exists to prevent.

### `DISTINCT` in an extreme

- **Was:** `min(DISTINCT x)` and `max(DISTINCT x)` of `integer` and
  `bigint` keep a table of the pairs of the group and the value, and
  the rows go into the aggregate only where they inserted their pair.
  The table must fit `hash_mem`, or with `GROUP BY` and a word for
  every argument it goes to disk. Of the other types, and `bool_and`
  and `bool_or` of any, such an aggregate leaves the whole grouping to
  the core.
- **Will be:** an aggregate the node computes as an extreme, of any
  type above, ignores its `DISTINCT`: the planner takes it as without,
  the node keeps no table of pairs for it, and the limit of `hash_mem`
  for the pairs leaves it alone. One function answers whether an
  aggregate's `DISTINCT` matters, and every place that reads
  `aggdistinct` asks it. The answer is the same: the least of the
  distinct values is the least of all, and a NULL is skipped either
  way.
- **Refused:** the extremes of the new types in the list of `DISTINCT`
  aggregates with their table of pairs. It costs a table where none is
  needed, and `money` has no hash function for a table outside the
  word.
- **Refused:** ignoring `DISTINCT` in any aggregate with a sort
  operator, as the core's planner does for an index. Of `text` or
  `numeric`, two equal values may differ in their bytes, `'a'` and
  `'A'` under a case-insensitive collation, `1.0` and `1.00`. Without
  `DISTINCT` the answer is the equal value met first; with it, the one
  the core's sort keeps. They may differ.

## What we do not do

- **A join by `money`.** It would need a plan of a hash join that the
  core does not make, for an equality it does not hash.
- **`oid` and `"char"`.** An `oid` is unsigned: in a word it would come
  out with its sign extended, unlike PostgreSQL's value. `"char"` goes
  with it, as the roadmap's entry `table-key-types` says.
- **Columns of `time` and `money` in `tessera-crosscheck`.** The tool
  makes none; the SQL suites compare these types with Tessera on and
  off.
- **No new code in Rust, no `unsafe`.** The kernels' code does not
  change, so its machine code does not either: no `--disasm` run.

## Measurement plan

On the M5 Pro, PostgreSQL 19 with `pg_pmu`, a release build. The base
is the commit of task 2.1, the cases without the change. Cycles
decide; instructions stand beside them; pg_duckdb's cycles stand beside
ClickBench's. Each run takes five minutes at most and waits for the
maintainer's word.

### The cases

`bench/pg/setup.sql` makes a table `bench_clock` of 2 million rows at
scale 1: `tm` a `time` of 1 440 minutes, `mo` a `money` of 1 000
values and `v` an `integer`, in no order; and `bench_minutes`, the
1 440 minutes with a number `w`. The other cases read `bench_dates`:
`d` a `date`, `ts` a `timestamp`, `s` a `smallint` of 1 000 values, `b`
a `boolean`, `v` an `integer`.

The family `anyagg`:

- `g_minmax_date`, `g_minmax_timestamp`, `g_minmax_int2`: `min` and
  `max` of `d`, of `ts`, of `s`;
- `g_bool_and_or`: `bool_and(b)` and `bool_or(b)`;
- `g_minmax_clock`: `min` and `max` of `tm` and of `mo`;
- `g_minmax_int4`, the twin: `min(v)` and `max(v)`;
- `g_group_minmax_dates`: `min(d)` and `max(ts)` by the 1 000 groups
  of `s`, both read by the outer query so that the planner keeps both;
  `g_group_minmax_int4`, its twin, with `min(v)` and `max(v)`;
- `g_minmax_int4_distinct` and `g_group_minmax_int4_distinct`: the two
  twins with `DISTINCT` in each aggregate.

The family `wordkey`:

- `key_time_group`, `key_money_group`: a grouping by `tm`, by `mo`;
- `key_time_distinct`: `DISTINCT` of `tm`;
- `key_time_join`: `bench_clock` joined with `bench_minutes` on `tm`;
- `key_clock_filter`: a count of the rows of `tm < time '12:00'` and
  `mo > 500::money`;
- `key_money_order`: a sort by `mo` skipped with `OFFSET`;
- `key_date_group`, already in the family, the control.

Runs: `cargo ab measure --base <the commit of 2.1> --family <family>
--cases <regex> --counters --cycles --rounds 2 --deadline 300` for each
family, then once more without `--cycles` for the instructions.
`plans.txt` shows the plans of both sides.

Hypotheses, written before the runs:

- `g_minmax_date`, `g_minmax_timestamp`, `g_minmax_int2`,
  `g_bool_and_or` and `g_minmax_clock`: at least 30 per cent fewer
  cycles; the first four within 10 per cent of their twin's cycles at
  the end;
- `g_group_minmax_dates`: at least 20 per cent fewer cycles, and within
  10 per cent of its twin's at the end;
- the two cases of `DISTINCT`: at least 30 per cent fewer cycles, and
  within 5 per cent of their twins' at the end;
- `key_time_group`, `key_time_distinct`: at least 25 per cent fewer
  cycles, a word in place of a dictionary;
- `key_time_join`: at least 20 per cent fewer, a word in place of a
  hash and a join filter;
- `key_clock_filter`: at least 30 per cent fewer, the kernels in place
  of a call a row;
- `key_money_group`: at least 50 per cent fewer, the node's hashed
  grouping in place of the core's sort and grouping;
- `key_money_order`: at least 10 per cent fewer;
- the twins `g_minmax_int4`, `g_group_minmax_int4` and the control
  `key_date_group`: within 2 per cent.

### ClickBench

`hits.parquet` in the page cache: `cargo clickbench run --counters
--tries 5 --mode on --workers 0`, one query a run, the base and the end
in turns, twice. pg_duckdb's cycles at one thread are those of its runs
of 2026-11-24, of the same server and file.

Queries: Q7, `min` and `max` of `EventDate`, the only query of the 43
with an extreme of a date or a timestamp; Q1, which reads no column,
and Q3, sums and a count of integers on the node's own path, the
controls.

Hypotheses:

- Q7: 40 to 55 per cent fewer cycles, of the generic path's 55 per
  cent of the profile less the kernels' own pass; fewer instructions;
- Q1 and Q3: within 2 per cent of cycles.

### How a query or a case is judged

- A side's spread is the difference of its two turns. A change of
  cycles smaller than the larger spread of the two sides is no change.
- When a control or a side spreads past 2 per cent, that query or case
  is counted once more; still past it, it is reported unsettled, not
  passed or failed.
- The change is kept when Q7 loses at least 30 per cent of its cycles,
  each case reaches its hypothesis's threshold, and no twin or control
  gains more than 2 per cent.
- A case whose plan differs from the base's, as `key_money_group`'s
  will, is judged by the cycles of the whole query and reported with
  both plans.
- A confirmed miss is profiled and searched as `CONTRIBUTING.md` says
  before the maintainer is asked.
- No times: the cycles decide, and the times come after the work on
  cycles.

The report is kept under `docs/benchmarks/`, as `docs/measuring.md`
says.

## Risks / Trade-offs

- [A plan changes where the generic cost made the core win] → the
  aggregates cost what the node's integer extremes cost, which are
  measured; the suites' plans show the change, and the cases'
  `plans.txt` the plans of the families.
- [A parallel grouping now takes these extremes in its partial stage,
  which it refused as generic] → `test/sql/parallel.sql` compares the
  results with and without workers, by groups and without them.
- [An infinite value, or the edge of a range, ordered wrong] → the
  suites compare `-infinity`, `infinity` and the least and greatest
  finite values with Tessera on and off, and `time` of `00:00` and
  `24:00`, and `money` of both signs.
- [A key of `time` loses its join filter] → equal times are equal
  words, as equal dates are; `test/sql/types.sql` joins by `time` with
  NULL and compares the rows with the core's.
- [`DISTINCT` ignored where it matters] → only in the extremes of the
  types above, where equal values are equal words; tests compare
  `min(DISTINCT x)` with the core's, with groups, with `FILTER`, and
  with a set of pairs that would pass `hash_mem`.
- [`EXPLAIN` changes] → a TessAgg stays a TessAgg; its `Kernel Calls`
  under `ANALYZE` count the new calls; a grouping by `money` becomes a
  TessAgg. The suites' expected outputs are read line by line.

## Decisions of the maintainer

- 2026-11-26: the change takes `smallint`, `bool_and`, `bool_or` and
  `every`, `time` and `money` as integers, and `DISTINCT` in the
  extremes; `time` and `money` and `DISTINCT` come in commits of their
  own, all in one pull request.
- 2026-11-25: cycles decide every count; instructions help but come
  second.
- 2026-11-24: counts on one version of PostgreSQL, 19, with
  pg_duckdb's cycles beside Tessera's.
- 2026-11-22: no times while the work on cycles goes on.

## Files

- `kernels/int.c`: the extremes registered; the comparisons of `time`
  and `money`.
- `nodes/agg_planner.c`, `nodes/agg.c`, `nodes/agg.h`: the node's own
  aggregates, the width of an extreme, `DISTINCT` in an extreme.
- `nodes/key_kind.h`, `nodes/join_planner.c`, `runtime/expr.c`: `time`
  and `money` as words.
- `test/sql/agg.sql`, `test/sql/parallel.sql`, `test/sql/types.sql`
  and their expected outputs.
- `bench/pg/setup.sql`, `bench/pg/anyagg.sql`, `bench/pg/wordkey.sql`,
  `bench/pg/README.md`.
- `docs/nodes.md`, `docs/function.md`, `docs/limitations.md`,
  `openspec/specs/hash-table/design.md`.

## Tests

- The extremes of each type above, with Tessera on and off
  (`agg_same`): without groups, with NULLs, with every row NULL, with
  no row; at the edges of each type's range and its infinities; with
  `GROUP BY`, with many groups, and past `hash_mem` (`agg_spill`).
- The plans: a TessAgg whose `Kernel Calls` count the calls of the
  extremes.
- With workers (`parallel_same`): without groups and with them, the
  partial stage taking the extremes.
- `time` and `money`: comparisons and lists of `IN` and `NOT IN`, keys
  of a grouping, a `DISTINCT`, a `UNION`, a join by `time` and a sort,
  with NULL and both signs, against the core's rows; a join by `money`,
  a `DISTINCT` of `money`, and a grouping by `bit varying` left to the
  core.
- `DISTINCT` in an extreme: against the core's answer, with groups,
  with `FILTER`, past `hash_mem`; `DISTINCT` in `min(text)` still the
  core's.
- `make crosscheck`, whose queries take `min` and `max` of `date` and
  `timestamp` with Tessera on and off.
