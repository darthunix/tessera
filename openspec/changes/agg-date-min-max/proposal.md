## Why

TessAgg computes `min` and `max` of `integer` and `bigint` itself: a
kernel finds the extreme of a batch's column, and with `GROUP BY` the
kernels of the hash table keep each group's extreme in its record. Of
`date`, `timestamp` and `timestamptz` it computes them through the
core's functions, the generic path: it calls the transition function,
`date_smaller` or `date_larger` for a date, once for each row. Yet a
value of these types is an integer, of four bytes for a date and of
eight for the two timestamps. The core compares them as integers, and
the node already reads them as integers for its comparisons and its
keys.

A profile of ClickBench's Q7, `SELECT MIN(EventDate), MAX(EventDate)
FROM hits` over the 100 million rows of `hits.parquet`, serial, found
32 % of Tessera's time in the generic path and the two functions. At
the end of `parquet-string-views` they took 55 % of a profile of Q7.
Q7 counted there 4.64 billion cycles and 33.3 billion instructions,
about 333 instructions a row; pg_duckdb counted 0.88 billion cycles,
so Tessera ran 5.3 times its cycles. A heap table takes the same path:
none of it is Parquet's own.

The same holds for the other aggregates whose answer is the first
value in the order of a type the node holds as an integer: `min` and
`max` of `smallint`, and `bool_and`, `bool_or` and `every`, the least
and the greatest of booleans held as 0 and 1. Two more types are
integers of eight bytes that the node does not hold as integers at
all: `time`, microseconds since midnight, and `money`, cents. Their
equality and order are those of an `int64`, but the node compares
them, groups and joins by them through their own functions, and
leaves a grouping by `money` to the core, as `money` has no hash
function.

`DISTINCT` does not change the answer of such an aggregate: the least
of the distinct values is the least of all. The node still keeps a
table of the distinct values for `min(DISTINCT x)` of an integer, and
leaves it to the core for the other types. PostgreSQL's own planner
ignores `DISTINCT` in these aggregates when it takes their answer from
an index.

The change takes up the roadmap's entry `agg-date-min-max`, first in
its queue, and the maintainer widened it on 2026-11-26 to these types
and to `DISTINCT`. It is work on speed, so it comes as a change with a
measurement plan.

## What Changes

- **The extremes by the kernels.** `min` and `max` of `smallint` and
  `date`, and `bool_and`, `bool_or` and `every`, go to the kernels of
  the extremes of `integer`; `min` and `max` of `timestamp` and
  `timestamptz` to those of `bigint`. On every path the extremes of
  `integer` take: without `GROUP BY`, with it, in the partial and
  final stages of a parallel plan, and in the groups that go to disk
  past `hash_mem`. The planner costs them as the node's own.
- **`time` and `money` as integers.** The node holds both in a word of
  64 bits, as a `bigint`:
  - their comparisons, `=`, `<>`, `<`, `<=`, `>` and `>=`, and the
    lists of `IN (...)` and `NOT IN (...)`, go to the kernels of
    `bigint`;
  - they are keys of the hash table as their values: a grouping, a
    `DISTINCT`, a `UNION` and a join by `time`, and a grouping by
    `money`, which the node now takes from the core;
  - a sort by either orders their words;
  - `min` and `max` of both go to the kernels of `bigint`.
  A join by `money` stays the core's: PostgreSQL does not mark its
  equality as one a hash join can use, so no plan of a hash join is
  made for it. A `DISTINCT` or a `UNION` of `money` stays the core's
  as well: the node offers its plan in place of the core's hashed one,
  which `money` does not get.
- **`DISTINCT` in an extreme ignored.** In `min`, `max`, `bool_and`,
  `bool_or` and `every` of the types above, the node computes the
  aggregate as without `DISTINCT`: no table of distinct values, no
  spill of it, and no limit of memory it set.
- The answers stay the core's. A value of these types orders as its
  integer, the infinities too, and two equal values are the same
  integer, so which of them an extreme keeps does not show.
- The families `anyagg` and `wordkey` get cases of each part, beside
  twins of `integer` that the change does not touch.

Not in this change:

- A join by `money`: it would need a plan of a hash join that the
  core does not make, for an equality it does not hash.
- `DISTINCT` in `min` and `max` of the other types, such as `text` and
  `numeric`: two equal values of them may differ in their bytes, and
  which one the answer shows then depends on the order of the rows.
- `oid` and `"char"`: an `oid` is unsigned, and in a word it would come
  out with its sign extended, unlike PostgreSQL's value.
- Types of `tessera-crosscheck`: it makes no columns of `time` or
  `money`; the SQL suites test them.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

- `hash-table`: the requirement "The SQL types of a key" lists `time`
  and `money` among the types put in as their values, and the types
  left to the core change: a grouping by `money` is the node's, a join
  by `money` stays the core's, and `bit varying` becomes the example of
  a type without a hash function.

No capability describes TessAgg or the kernels' comparisons yet: as
`CONTRIBUTING.md` says of a part without a folder, they are described
in `docs/nodes.md` and `docs/function.md`, and this change edits that
description.

## Impact

- Code:
  - `kernels/int.c`: the extremes of the types above registered with
    the codes of `integer` and `bigint`; the comparisons of `time` and
    `money` with those of `bigint`;
  - `nodes/agg_planner.c`: the node's own aggregates, and `DISTINCT`
    in an extreme; `nodes/agg.c`: the width of an extreme, and no
    table of distinct values for an extreme;
  - `nodes/key_kind.h`: `time` and `money` as keys of eight bytes;
    `nodes/join_planner.c`: the equality of `time` as one of words;
    `runtime/expr.c`: the lists of `IN` of both types.
  No Rust code changes. No new kernel, no `unsafe` code.
- Public headers: none. The kernels' entry points, the hash table's
  kinds of keys and of states stay as they are.
- Settings: none.
- SQL suites: `test/sql/agg.sql`, `test/sql/parallel.sql` and
  `test/sql/types.sql`, with their expected outputs.
- Benchmarks: the families `anyagg` and `wordkey`, a table of `time`
  and `money` in `bench/pg/setup.sql`, and their entries in
  `bench/pg/README.md`.
- Documents: `docs/nodes.md`, `docs/function.md`,
  `docs/limitations.md`, and the design of `hash-table`.
- Capabilities: `hash-table`, one requirement. Contracts: none touched;
  `hash-table` has none.
- Roadmap: the entry `agg-date-min-max` leaves it at the close, and
  the entry `types-outside-tpch` no longer names `time`.
