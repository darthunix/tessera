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
- **Tables other than heap tables.** A foreign table keeps its wrapper's
  scan unless a registered source reads it in batches
  ([source.md](source.md)), as the Parquet wrapper's source reads its
  tables ([parquet.md](parquet.md), and below); tables of another
  access method and `TABLESAMPLE` keep the core's scan. A foreign table
  with lateral references, a pseudoconstant clause, or only paths with
  parameters or path keys is offered to no source.
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
  equality hashes, for `sum` and `avg` of integers
  (`distinct_supported`), and for the extremes the node computes as its
  own, which ignore it (`distinct_matters`); any other, such as
  `sum(DISTINCT numeric)` or `min(DISTINCT text)`, leaves the whole
  aggregation to the core.
- **A grouping with `DISTINCT` past `hash_mem`**: its pairs go to disk
  only with `GROUP BY` and an argument a word holds (`smallint`,
  `integer`, `bigint`, `date`, `boolean`, `timestamp`, `timestamptz`,
  `time`, `money`).
  The pairs of a text or `numeric` argument, and the set of an aggregate
  without `GROUP BY`, stay in memory: the node takes such a grouping only
  when the planner's estimate of its pairs fits `hash_mem`. The groups of
  a grouping with `DISTINCT` never go to disk: the node takes it only
  when the estimate of its groups with their states fits `hash_mem`, and
  half of it when its pairs do not fit. Such a grouping has no parallel
  form.
- **Aggregates of other extensions** whose functions are C functions of a
  loadable library, and aggregates whose arguments contain a subquery
  (`generic_supported`).
- **More than 16 grouping keys** in one query, and a grouping whose
  keys, states' arguments and `FILTER` conditions pass 1 664 computed
  columns or whose group's record passes 64 kB, the table's first chunk:
  the states of the aggregates the node folds count once.

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

## Parquet foreign tables

The wrapper `tessera_parquet` ([parquet.md](parquet.md)) reads local
Parquet files; its suite is `test/sql/parquet.sql`, whose cases are
named below.

- **Files on the server's file system alone**, named by absolute paths
  or patterns of the shell; no object store, no URL, and no path that
  holds a comma. A table is read-only, and its columns are declared by
  hand: there is no `IMPORT FOREIGN SCHEMA`.
- **Codecs**: chunks uncompressed or compressed with Snappy. A query
  that reads a column compressed another way, such as GZIP or ZSTD,
  fails; one that reads only the file's other columns does not
  (case "Options refused").
- **Types**: no `INT96`, decimal, time of day, time in nanoseconds,
  unsigned 64-bit integer, list, map, group or repeated column, and no
  bytes marked as JSON or a UUID; no declared `numeric`, `char(n)` or
  `timestamp(n)`. A query fails on such a column only when it reads it
  (case "Columns refused"). A `text` or `varchar` column needs a
  database of the encoding `UTF8` (case "A database of another
  encoding").
- **A whole row and the system columns**, such as `SELECT t FROM t` or
  `tableoid`, are read by the wrapper's scan, row by row, not by the
  source (case "A whole row and a system column").
- **Clauses the source leaves to `TessFilter`**: any clause outside the
  source's list ([parquet.md](parquet.md), "What the source checks"),
  such as `ILIKE`, a regular expression, a `LIKE` with `_`, an escape
  or more than 31 `%`, a comparison of two columns, of floating-point
  numbers or of the order of strings, a `timestamp` compared with a
  `timestamptz`, and clauses on strings under a nondeterministic
  collation (case "Clauses left to the filter").
- **Skipping by the footer** is by whole groups of rows only: no page
  index, no Bloom filter, no row skipped inside a group. Of strings only
  the emptiness of the least and greatest values is used, for `= ''`
  and `<> ''`; a dictionary only where the footer says every data page
  of the chunk is encoded through it; the older least and greatest
  values only of signed columns (case "Groups the footer cannot rule
  out").
- **Parallel reading** divides the files by groups of rows: a file of
  one group is read by one process.
- **Errors of values the source never makes.** With Tessera off the
  wrapper makes every value of every column a query reads, so a string
  that is not UTF-8 or a time out of range in a row a clause rejects
  stops the query; the source's batches make only the values asked and
  do not (case "Bytes checked when they become values").
- **Changes of a file that keep its size and time.** A scan compares a
  file's size, time of modification, device and inode with what it
  found when it began. A file rewritten in place with its size and time
  kept, as `cp -p` or `rsync -t` may do, written through a shared
  `mmap`, whose time changes only at `msync` or writeback, read over
  NFS, whose answers come from a cache of attributes for seconds, or
  written within one tick of a coarse clock, as Linux before 6.13 gives
  a file's time, is not seen, and the rows may then be wrong. A file
  replaced by a rename is seen (case "Files replaced, added, removed and
  damaged").
- **The cache of footers** of a backend holds a thousand files, the
  least recently used dropped first; a backend that reads more files
  than that, query after query, reads their footers again.

## Platforms

- **PostgreSQL 18, 19 and master** (20devel): the modules stop the build
  on an older core. Where an interface differs between them, it is read
  in two ways where it is used. On 19 and 18 the node does not group a
  UNION by the workers where master's would: they estimate a UNION's
  rows as all its branches' rows, master sums their distinct groups, and
  the node takes the core's estimate. On 18 the node takes no column as
  present and not NULL in every row, which 19 promises for the columns
  of a table scan; its heap batches count the columns' fixed offsets
  themselves and jump by them, but a column read behind a row's cursor
  and a fixed-width one passed by reference are walked to from the
  tuple's start or the cursor, where 19 jumps. Plans can differ between
  the cores where the cores price differently, such as 19's estimate of
  a hash join's buckets; the rows are the same. Greengage 7 is a design
  in the plan's section 7, with no code yet.
- **Vector kernels on AArch64** (NEON) and on x86-64 with AVX2 and
  POPCNT, chosen at run time where the processor has both; elsewhere the
  scalar kernels run, with the same results. AVX-512 is not used. The
  AVX2 code was measured on one Intel processor, of the Ice Lake
  generation, and not yet on AMD's.
