# What stays with the core

Tessera adds a batch node only where it gives the result the core's node
would; everywhere else the core's own node runs, and the two mix in one
plan: a core node reads a batch node's rows, and a batch node reads a core
node's rows through `TessPack`. This page lists what stays with the core in
this revision. The coverage suite (`test/sql/coverage.sql`) prints, for
each condition and query form it lists, whether it runs in batches, row by
row inside a batch node, or with the core's nodes, so a change of any line
below shows there; an item without a case there names where the planner
decides. The [nodes guide](nodes.md) explains each node's planning.

## Statements

- **Statements that write or lock rows.** In `INSERT`, `UPDATE`, `DELETE`
  and `SELECT … FOR UPDATE` or `FOR SHARE`, the statement's own scans
  stay the core's (`relation_supported` in `nodes/scan_planner.c`). A
  subquery planned on its own, such as the grouping of
  `INSERT … SELECT … GROUP BY`, can use the batch nodes.
- **Tables other than heap tables.** Foreign tables, tables of another
  access method, and `TABLESAMPLE` keep the core's scan.
- **Parameterized paths.** A batch path is never parameterized by an outer
  row (`check_path_config` in `runtime/planner.c`), so the inner side of a
  nested loop that probes an index for each outer row, and a scan with
  lateral references, stay the core's. For INNER, LEFT, SEMI and ANTI
  joins the hash join looks up the keys of each outer batch in a btree
  index instead, where the cost model finds it faster than the core's
  nested loop ([nodes.md](nodes.md), TessHashJoin, Index lookups); keys
  that are not an index's first column or that are an expression,
  partial indexes and tables past `effective_cache_size` keep the
  core's nested loop. A lookup reads every row of a key, where the
  core's nested loop of a SEMI or ANTI join stops at the first that
  passes.

## Conditions and expressions

A clause the [expression compiler](expr.md) takes runs in batches, over the
kernels of the function registry. Any other clause runs row by row inside
`TessFilter`, through the core's interpreter: for example `ILIKE`, regular
expressions, `upper` and `lower`, string concatenation, `md5`,
`date_trunc` of a date, `round` of a numeric, interval comparisons, and
text ordering comparisons under a collation; the suite's `row` lines list
those it checks.

## Aggregation

- **Grouping sets** (`ROLLUP`, `CUBE`, `GROUPING SETS`), **queries with
  window functions** and **`DISTINCT ON`**: the core's aggregation
  (`query_supported` in `nodes/agg_planner.c`).
- **Ordered-set aggregates** (`WITHIN GROUP`) and **`ORDER BY` inside an
  aggregate**.
- **`DISTINCT` inside an aggregate** only for `count` of a type whose
  equality hashes and for `sum`, `avg`, `min` and `max` of integers
  (`distinct_supported`); any other, such as `sum(DISTINCT numeric)`,
  leaves the whole aggregation to the core.
- **Aggregates of other extensions** whose functions are C functions of a
  loadable library, and aggregates whose arguments contain a subquery
  (`generic_supported`).
- **More than 64 aggregates or more than 16 grouping keys** in one query.

## Sorting

- `TessSort` replaces the sort of the query's `ORDER BY`, also each
  participant's under a `Gather Merge`. Sorts inside the plan, such as
  those under a merge join, a window function or `DISTINCT ON`, stay the
  core's.
- **`ORDER BY … LIMIT` whose first key is `float4` or `float8`**: the
  core's bounded heap of tuples was measured faster than the node's for
  types passed by value without an abbreviated key
  (`nodes/sort_planner.c`).
- **`FETCH FIRST … WITH TIES`** and **incremental sort**.

## Joins

- `TessHashJoin` takes the hash joins: inner, left, right, full, semi,
  anti and right anti. A join without a hash-joinable equality clause
  stays a core nested loop or merge join, as does any join with
  `enable_hashjoin` off.
- **Right semi joins**, which the core chooses when hashing a semi
  join's outer side is cheaper (`join_planner.c`); the suite's table
  gives no such plan.
- **Pruning by a join's keys** is done by inner, semi, right and right
  anti hash joins, by a key of an integer, date, boolean or timestamp
  type on the first column of a partition key; left, anti and full
  joins, and keys of other types, read every partition
  ([partition-pruning](../openspec/specs/partition-pruning/spec.md)).

## Execution

- **Backward scans and mark/restore.** No batch node supports them; the
  core puts a `Material` node above a batch node where a `SCROLL` cursor or
  a merge join's inner side needs them.
- **Window functions** run in the core's `WindowAgg`; there is no batch
  node for them.
- **Errors of rows the core's plan does not evaluate.** A batch node
  evaluates an expression for a batch of 64 rows at once, past a LIMIT
  too, and its plans differ from the core's. It groups every row under a
  LIMIT, where the core's sorted grouping stops early, and it may hash a
  join on a condition the core checks only after a match. A data error
  (an overflow, a division by zero) in a row the core's plan never
  reaches can then stop the query with Tessera, and the other way round.
  The core makes no promise either: a parallel plan or another join order
  raises the same errors. A `CASE` guards an expression that may fail, as
  [PostgreSQL's evaluation rules](https://www.postgresql.org/docs/current/sql-expressions.html#SYNTAX-EXPRESS-EVAL)
  advise.
- **Reading ahead of a join or an aggregate.** TessHashJoin and TessAgg
  gather the rows of sparse batches, a filter's output a row or two a
  batch, into batches of 64, so they read their child until 64 rows
  have come, which after a selective filter may take thousands of rows.
  A limit right above the join turns its gathering off, and the first
  batch of every scan goes as it is. Elsewhere the reading goes further
  than a batch at a time: under `EXISTS` a join may drop several batches
  before its first pair, and a limit does not reach a join or a filter
  below the node right under it. A data error in a row the core's plan
  never reaches can then stop the query, as above.
- **The calls of volatile functions.** Batch nodes evaluate a volatile
  function (`random()`, `nextval()`, a function that writes) as often as
  PostgreSQL promises, at every row where its value is needed, but the
  count over a join and the order between nodes are the plan's, and
  differ from the core's. A filter runs over a batch's rows before the
  node above reads any of them, and over many batches before a gathering
  join or aggregate does. The index lookup of TessHashJoin looks a
  batch's keys up in one scan of the inner side, so a volatile condition
  of the inner side runs once for an inner row that several outer rows
  look up, as in the core's hash join and unlike its nested loop, where
  it runs for each. The core's own plans differ the same way.

## Platforms

- **PostgreSQL master** (20devel) only: the modules stop the build on an
  older core. PostgreSQL 19 needs its own forms of three interfaces
  (working plan, item 8.8); Greengage 7 is a design in the plan's section
  7, with no code yet.
- **Vector kernels on AArch64** (NEON) and on x86-64 with AVX2 and
  POPCNT, chosen at run time where the processor has both; elsewhere the
  scalar kernels run, with the same results. AVX-512 is not used. The
  AVX2 code was measured on one Intel processor, of the Ice Lake
  generation, and not yet on AMD's.
