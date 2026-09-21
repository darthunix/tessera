# Nodes module

`tessera_nodes` (`nodes/`) is the module of Tessera's own batch nodes. It
links the runtime library statically and, when loaded, registers its node
kinds in the bridge's node registry and its scan methods with PostgreSQL.
The pack and heap scan nodes below are created by batch parents and need
no hook; the filter node offers its path to base relations through the
module's `set_rel_pathlist` hook, the aggregate node to the grouping stage
through its `create_upper_paths` hook. It is loaded after the bridge; loading it without
the bridge is an error. A
running installation preloads both in every session (see
[bridge.md](bridge.md)):

```
session_preload_libraries = 'tessera, tessera_nodes, tessera_kernels'
```

Tests and one-off sessions load them by hand instead:

```sql
CREATE EXTENSION tessera;
LOAD 'tessera_nodes';
```

Every node here keeps the obligations of [node.md](node.md) and is built
with the runtime library's helpers described in [runtime.md](runtime.md).

## Independent example: TessLimit

`examples/limit/` is a node built outside this module, against the public
headers and the runtime library alone, as a node of another extension
would be: the module `tessera_limit` registers the kind `tessera.limit`,
installs its own `create_upper_paths` hook and replaces the core limit path
in the final relation with a batch limit above a batch input over the
limit's child, so an ordinary child gets a pack node below. It stands on
the unary helper, removes the offset's rows and the rows past the count,
stops the input once the count is reached, and passes the tuple bound to
the child through the pack node, which keeps a sort under it a top-N sort.
`WITH TIES` and parameterized inputs stay with the core node. Load it
after the bridge, and after the nodes module when ordinary children should
be packed; without a pack node it adds no paths. The
[node-writing guide](writing-a-node.md) walks through it.

## TessHeapScan

`TessHeapScan` reads a plain heap table in batches for a batch-aware
parent, in place of the pack node over the core's sequential scan, which
returned every row through `ExecProcNode` into a slot. The chain of the
first queries is therefore `TessHeapScan → TessFilter → parent`.

### Planning

The node publishes `scan_rows`: a path over a sequential scan of a plain
heap table (`RELKIND_RELATION`, the heap access method, no inheritance, no
sampling) whose targets are its columns. `tess_batch_scan_path` builds it
for a parent that evaluates the relation's clauses itself, as the filter
does, and `tess_batch_input_path` prefers it to the pack node for a
relation without clauses. A pseudoconstant clause keeps both helpers
away: the planner would gate every scan of the relation with a `Result`
between the parent and the node. The path copies the scan's costs and
parallel safety (the node is not parallel-aware; a single-copy `Gather`
runs it whole in one worker) and estimates every row of the relation,
since the node evaluates no clause. `PlanCustomPath` builds an explicit
layout with every attribute of the relation a batch column, dropped ones
included, and each target mapped to its column; `scanrelid` names the
relation, which the executor opens and closes.

### Execution

The scan is begun at the first execution, with the query's snapshot, once
the parent's request is frozen; a request for rows is an error. Each
batch comes from one page: the node lets the core's scan bring the next
page in, prune it and decide which tuples are visible, with one
`heap_getnextslot` call per page, and takes the page's visible tuples
straight from the scan's list into a heap batch (`TessHeapBatch`,
[runtime.md](runtime.md)) in one call, up to the batch size, resuming on
the next call with the rest of the page; before moving on, it sets the scan's position
past the page so that the core fetches another. The batch pins the page
until it is released, so a column is deformed only when a consumer asks
for it, for the rows it asks for, and by-reference values, external
TOAST pointers included, are read from the page as a slot would give them.
A scan the core does not run in page mode, which a non-MVCC snapshot
would give, is read one tuple at a time through the same batch. Rescan
clears the output and restarts the scan; a bound from a limit above stops
the scan after as many rows. Backward scan and mark/restore are refused,
as for every batch node.

`EXPLAIN` shows `Batch Size` once executed and, with `ANALYZE`, the
`Batches`, the `Pages` read, the `Deformed Datums` and the `Restarted
Datums`; the row counts are corrected by the output helper.

### Tests

The filter and limit suites run through the scan; the filter suite adds a
table with dead tuples and an aborted insert, a page with more visible
tuples than a batch, values stored outside the page, a column missing
from older tuples, and a clause the planner folds away.

## TessPack

`TessPack` turns the rows of an ordinary child into batches. It is the
boundary between PostgreSQL's row-wise plans and Tessera's batch nodes, so
that a batch parent can stand above any core path without a patch to
PostgreSQL. Above a plain heap table `TessHeapScan` takes its place; the
pack remains for any other child: a sort, an aggregate, a scan the heap
scan refuses.

### Planning

A parent never creates the pack path directly. It asks
`tess_batch_input_path` for a batch child over the path it has, and when
no node reads the relation natively the helper calls the `wrap_rows`
callback the pack node registers under `tessera.pack`. The pack path copies its child's planner properties: rows,
costs, path keys and parallel safety. There is no cost model yet, so the
path costs exactly what its child costs. The pack path exists only as a
child of a batch parent; the module adds it to no path list.

When the child is a sequential scan of a plain table whose targets are all
columns of it, `wrap_rows` plans the scan with the relation's physical
target list (`build_physical_tlist`): the executor then finds nothing to
project and the scan returns its buffer tuple slot. The pack's own target
list stays the one its parent gave it, so a parent that preserves the
pack's layout keeps working, and `PlanCustomPath` builds an explicit layout
with every relation column as a batch column and each target mapped to its
column. A table with dropped or missing columns, whose scan must project,
or any other child keeps its own target list, and the plan uses the dense
layout: the child was planned with its exact target list, so child
attribute N is target N. In both cases the pack has no qualifier of its
own: the relation's clauses are evaluated by the child, or taken over by a
filter above. A pack path is never parameterized, since the path helper
refuses such a template.

### Execution

`BeginCustomScan` reads the plan, initializes the child and binds the
result slot through the output helper. The batch provider is created at
the first execution, once the parent's request is frozen and the child's
first slot is known: PostgreSQL initializes the plan top-down, so the
parent sends its request only after the pack node's `BeginCustomScan` has
bound the slot. A buffer tuple slot gets a heap batch (`TessHeapBatch`,
[runtime.md](runtime.md)): the rows are kept as tuple headers pointing
into their pages, which stay pinned until the batch is released, and a
column is deformed only when a consumer asks for it, for the rows it asks
for, resuming each row from where an earlier request stopped. So a
filter's batch clause deforms its column for every row, its residual and
the columns a parent reads only for the rows that survived, and nothing
is copied. Any other slot gets the builder, which copies every column of
every row. The batch size is the request's `max_batch_rows` capped at 64
rows, or 64 when the request leaves it open. A request for rows is an
error: the pack node serves batch-aware parents only, and a row-wise
parent never plans one.

Each execution first returns the previous batch to its provider, which is
an error while the parent has not finished it, then fills up to the batch
size from the child and publishes the batch; once the child is exhausted,
it publishes the remaining rows and afterwards returns nothing. The
request's column masks are still not read: laziness comes from the
consumers' own column requests. Rescan clears the output, rescans the
child and starts a fresh batch. The node is parallel-safe whenever its
child is and keeps no shared state. When the parent bounds the rows it
needs, through the node kind's `set_tuple_bound` callback, the pack node
forwards the bound to its child and pulls no more rows than that, so a
sort below stays a top-N sort and the last batch may be short.

`EXPLAIN` shows `Batch Size` and `Rows Kept As` (`heap tuples` or
`copies`) once the node has executed, since both follow the parent's
request and the child's slot, and with `ANALYZE` the number of `Batches`
and, for kept tuples, the `Deformed Datums`, the `Restarted Datums`
(values before a row's cursor, deformed from the row's start) and any
`Copied Tuples` from slots without a page; the row counts are corrected by
the output helper, so the node reports the rows it packed.

### Tests

The [pack test](../test/tessera_pack_test.c) is a stand-in for a
batch-aware parent built on the unary helper: its hook wraps the sequential
scan of every table named `pack_*` through `tess_batch_input_path` and puts
the sink above it, which serves the rows of the batches to the executor.
With `pack_test.trim` set, a trim node of the same kind stands between
them: it keeps the rows whose first column is at most the setting, forwards
the pack node's batches to the sink, and shows the request the helper
derived. The SQL scenario shows the plan and its rows with NULL values,
batch sizes following the request and the cap, an empty child, an early
stop by a limit above, rescan through a correlated subplan, a scrollable
cursor that PostgreSQL serves through Material, a parallel worker, the
enable switch, the trim node removing rows and skipping emptied batches,
stopping after its first batch and passing the smaller batch limit down,
and the error of a parent asking for rows. The GUCs `pack_test.batch_rows`,
`pack_test.trim`, `pack_test.stop` and `pack_test.rows_mode` drive it.

## TessFilter

`TessFilter` applies the clauses of a base relation to batches: the leading
clauses the [expression compiler](expr.md) supports run as batch filters
over the function registry, the rest row by row over the rows those kept.
It is the first node of the chain that does work on batches, and until a
native batch scan exists it stands above a pack node above the sequential
scan: `Seq Scan → TessPack → TessFilter → parent`.

### Planning

The module's `set_rel_pathlist` hook, after the hook it replaced and the
enable switch, considers a plain `SELECT` over one heap table without
inheritance, sampling, row marks or lateral references, with at least one
clause and none pseudoconstant, since the planner gates every scan of such
a relation with a `Result` node that would stand in place of the node's
children. The clause the planner evaluates first must be a batch filter,
which needs the kernels module loaded at planning time: `order_qual_clauses`
is private, so the hook repeats its key, the lowest cost within the lowest
security level, a cheap leakproof clause counting as level zero, the first
of equals. With a non-parameterized sequential scan in the path list, the
hook adds a path over a batch input over a copy of that scan, at nine
tenths of the scan's cost: there is no cost model yet, and the node is
expected to lose until the native scan arrives (see `bench/pg/`).

Two things make the node possible before that scan. The planner gives
every scan of the relation its clauses, so `PlanCustomPath` takes them
away from the sequential scan below the pack node, after checking they are
the ones it received, and the scan produces every row. And the columns the
clauses need but the query does not are absent from the relation's target,
so the scan below is planned with a target extended by them, while the
node keeps the relation's own target through an explicit layout that maps
each target to the child's column and hides the rest. The clauses arrive
in the planner's order; the leading run the compiler supports becomes the
batch prefix in `custom_exprs`, the rest the plan's qualifier, so a cheap
guard stays in front of the division it protects and a security qualifier
in front of what it hides. The node computes no projection: PostgreSQL
puts a `Result` above it when the query needs one.

### Execution

`BeginCustomScan` compiles the batch prefix against the child's layout,
since PostgreSQL rewrites the clauses to reference the scan tuple, which is
the child's target list, and stands on the unary helper with the clauses'
columns as its filter columns. Per batch, the expression context is reset
once, each batch clause is bound and applied in order over the rows the
previous ones left, and the row-wise clauses, when there are any, run over
the survivors: their columns are fetched with the narrowed mask, each row
is shown to `ExecQual` through the scan tuple slot, and a false result
clears the row's bit. A batch-aware parent receives the child's slot with
the whole batch; an ordinary parent receives rows through the helper. The
node forwards no tuple bound, since it removes rows.

`EXPLAIN` shows the batch prefix as `Batch Filter` and the rest as the
core's `Filter`; with `ANALYZE`, the rows removed by each part, per loop,
and the helper's batches and rows. The core's `Rows Removed by Filter`
counts both parts, since the helper reports every row the node removes.

### Tests

`test/sql/filter.sql` compares the rows of every query with Tessera on and
off through one function: every comparison, a value chain, the commutator,
the unary minus, a column only the clauses read, no column at all, a
projection above the node, more than one batch, batches left empty, a
parameter and a NULL parameter through a generic plan, a pseudoconstant
clause, a correlated subquery, the limit node above, a residual text
comparison, the planner's reordering of a text comparison and of an
expensive predicate behind a cheaper int4 clause, a null test that keeps
the node away, the separate removal counts, a join of two filtered
relations, the planner's order in front of a division by zero, a parallel
worker, a scrollable cursor, an `UPDATE`, and the switch off.

## TessAgg

`TessAgg` computes the aggregates of a query without `GROUP BY` over the
batches of a batch child and returns the one result row, in place of the
core's plain `Aggregate`, which would receive the child's rows one at a
time: `TessHeapScan → TessFilter → TessAgg → parent`. It handles
`count(*)` and `count`, `sum`, `min` and `max` of an int4 column or of a
chain the [expression compiler](expr.md) accepts over one, with constants
and parameters; expressions above the aggregates and `HAVING` are left to
the plan's own projection and qualifier over the aggregates.

### Planning

The module's `create_upper_paths` hook, after the hook it replaced and the
enable switch, acts on the grouping stage of the main grouping relation
for a query with aggregates and no `GROUP BY`, grouping sets or window
functions, over an input that is not known to be empty. It collects the
aggregates of the target and `HAVING` and accepts them when every one is a
plain call, without `DISTINCT`, `ORDER BY` or `FILTER` and not split for
partial aggregation, of an aggregate the node combines and the
[function registry](function.md) implements over batches (kind
`TESS_FUNCTION_AGGREGATE`, registered by the kernels module), with an
argument the compiler supports whose columns the batch child's target
has. For each of the core's plain aggregate paths whose input can be read
in batches (`tess_batch_input_path`: a batch path as it is, a clause-free
sequential scan through `TessHeapScan`, anything else through `TessPack`),
the node's path takes the core path as its template at nine tenths of its
cost with the batch child, and `add_path` decides. `PlanCustomPath` makes
the distinct aggregates the scan tuple, `custom_scan_tlist` without a
relation, so that the planner turns the targets and `HAVING` into
references to it, and keeps one batch column per target. The arguments
travel in the private data with their columns resolved against the
child's target list, since `custom_exprs` would be fixed against the scan
tuple of aggregates.

### Execution

`BeginCustomScan` compiles the arguments, finds each aggregate's batch
function, asks the child for whole batches with the arguments' columns as
projection columns, so that a lazy provider deforms them for the surviving
rows only, and stands on the output helper over the result slot with a
one-row builder. The first execution reads every batch of the child: each
argument is bound to the batch, its column and the batch's rows go to the
batch function, and the partial it returns joins the running value, by
int8 addition checked for overflow (`bigint out of range`, as the core's
`int8inc`; the core's `int4_sum` does not check, which differs only past
four billion rows) for `count` and `sum`, by comparison for `min` and
`max`; `sum`, `min` and `max` stay NULL without a contributing row, `count`
is 0. The row is built in the scan slot, `HAVING` is evaluated over it, the
plan's projection runs when the targets are not the bare aggregates, and
the row is published as a one-row batch: a batch-aware parent such as
`TessLimit` reads the batch, an ordinary parent the row. The next call
returns nothing. Rescan passes changed parameters on to the child, since
the core does that for outer and inner plans only, and resets the values.
The node forwards no tuple bound, as the core's aggregate does not.

`EXPLAIN` shows `HAVING` as the core's `Filter`; with `ANALYZE`, the
batches and rows read from the child.

### Tests

`test/sql/agg.sql` compares the results of every query with Tessera on and
off through one function: the node above the native scan, above the
filter and above pack over a filtered scan and over a join; the same
aggregate twice, expressions above the aggregates, `HAVING` true and
false; the four column aggregates with `count(*)` over a filtered table,
the whole table, nothing and a column of NULLs; chains and a parameter
in the argument with a generic plan re-executed; a correlated subquery,
also with a hash aggregate under pack; a limit above reading the node's
batch; a scrollable cursor; a single-copy `Gather`; the overflow of a
chain; and the core keeping `DISTINCT` and `FILTER` in the aggregate,
`GROUP BY`, a window function, an empty relation, another argument type,
a cast, `avg`, the switch off and the kernels module absent.
