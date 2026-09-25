# Nodes module

`tessera_nodes` (`nodes/`) is the module of Tessera's own batch nodes. It
links the runtime library statically and, when loaded, registers its node
kinds in the bridge's node registry and its scan methods with PostgreSQL.
The pack and heap scan nodes below are created by batch parents and need
no hook; the filter node offers its path to base relations through the
module's `set_rel_pathlist` hook, the aggregate node to the grouping stage
through its `create_upper_paths` hook, the hash join node to joins
through its `set_join_pathlist` hook. It is loaded after the bridge; loading it without
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
sampling) whose targets are its columns or expressions over them. `tess_batch_scan_path` builds it
for a parent that evaluates the relation's clauses itself, as the filter
does, and `tess_batch_input_path` prefers it to the pack node for a
relation without clauses. A pseudoconstant clause keeps both helpers
away: the planner would gate every scan of the relation with a `Result`
between the parent and the node. The path copies the scan's costs and
parallel properties: over the core's partial sequential scan it is
parallel-aware with that scan's number of workers, since the node shares
the core's page handout and its counters, and estimates each
participant's share of the relation's rows by the core's divisor; over
the serial scan it estimates every row of the relation, since the node
evaluates no clause. It declares that the node projects:
a batch parent asked for a projection above a sequential scan takes the
scan node with the projection's target (`tess_batch_input_path`), and
PostgreSQL installs a projection it needs into the node's target list
instead of a `Result` above it. `PlanCustomPath` builds the projected
layout over the relation's row as the scan tuple, every attribute a batch
column, dropped ones included, and the targets are derived when the plan
is read, a column of the relation mapping to its column and any other
target becoming a computed column after them; `scanrelid` names the
relation, which the executor opens and closes.

### Execution

The parent's request is frozen at the first execution, and a request
for rows is an error. The scan is begun with the query's snapshot: in a
serial plan at that first execution; under a `Gather`, in the shared
memory callbacks, where the leader lays out the core's parallel scan
descriptor and the rows of the participants' counters
(`TessSharedStats`, [runtime.md](runtime.md)) in the node's chunk and
begins its scan before its first execution, and each worker attaches to
both. The descriptor hands every participant its own pages, so the page
loop below is the same in both plans. Each batch comes from one page: the node lets the core's scan bring the next
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
would give, is read one tuple at a time through the same batch. Computed
targets are the projection provider's ([runtime.md](runtime.md)): the
node wraps each heap batch before publishing it, and the provider
computes a column when a consumer asks, for the rows asked for, by a
batch chain or row by row over the relation's row in the scan tuple slot.
Rescan clears the output and restarts the scan; the shared handout of a
parallel scan starts over in the leader's reinitialization, which the
`Gather` runs before it launches the workers again. A bound from a limit
above stops the scan after as many rows. Backward scan and mark/restore
are refused, as for every batch node. When the executor shuts the node
down after the plan's last row, in the leader and in every worker, the
node stores its counters into its row and ends a parallel scan, whose
descriptor refers to the shared memory, while that is mapped; a serial
scan stays, since the executor shuts a plan down after a partial run of
it too, and a `Gather` a limit above stopped sets the node up anew when
it is rescanned.

`EXPLAIN` shows `Batch Size` once executed and, with `ANALYZE`, the
`Batches`, the `Pages` read, the `Deformed Datums`, the `Restarted
Datums` and, with computed targets, the `Computed Datums`; the row counts
are corrected by the output helper. In a parallel plan the leader shows
the totals over every participant, summed once the workers have
finished, so the `Pages` of a whole scan equal the relation's pages.

### Tests

The filter and limit suites run through the scan; the filter suite adds a
table with dead tuples and an aborted insert, a page with more visible
tuples than a batch, values stored outside the page, a column missing
from older tuples, and a clause the planner folds away. The parallel
suite (`test/sql/parallel.sql`) runs it under a `Gather` with two workers,
compares every result with Tessera off, checks that the participants'
pages add up to the relation's, with workers planned but not launched
and with the leader not taking part, rescans the `Gather` in a join,
stops it early with a limit and rescans it then, and passes a generic
plan's parameter and a worker's error through.

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
costs, path keys, parallel safety and number of workers, and clears
parallel awareness: over a parallel scan of the core each participant
packs its own rows, and the node shares nothing. There is no cost model
yet, so the path costs exactly what its child costs. The pack path exists only as a
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

When the child is a subquery scan without clauses, whose targets are all
columns of the subquery and whose subquery is planned as a batch path,
`wrap_rows` marks the path for forwarding: the pack packs nothing and
hands the batches of the plan under the subquery scan to its parent.
PostgreSQL keeps the subquery scan in the plan for the subquery's range
table, unless the scan is trivial, in which case it drops it after
planning and the pack's child is the batch node itself. The explicit
layout `PlanCustomPath` builds from the subplan's layout, mapping every
target to the column of the subplan's target of the same number, holds
either way. A subquery scan with a clause of its own, or over a subquery
whose plan is not a batch path, is packed as any other child.

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

When forwarding, the node stands on the unary helper without a process
callback: the batch source is the subquery scan's subplan, or the child
itself when the planner dropped the scan; the parent's request reaches the
source through the layout's map, the source's slot is returned as the
node's, and a rescan goes through the subquery scan, which is how a
changed parameter reaches the subquery's plan. The subquery scan itself is
never executed, and `EXPLAIN ANALYZE` shows it so. A bound from a limit
above reaches the source's node kind.

`EXPLAIN` shows `Batch Size` and `Rows Kept As` (`heap tuples` or
`copies`, or `forwarded batches` with the number of `Batches` forwarded
under `ANALYZE`) once the node has executed, since both follow the parent's
request and the child's slot, and with `ANALYZE` the number of `Batches`
and, for kept tuples, the `Deformed Datums`, the `Restarted Datums`
(values before a row's cursor, deformed from the row's start) and any
`Copied Tuples` from slots without a page; the row counts are corrected by
the output helper, so the node reports the rows it packed.

### Tests

The `forward` suite covers the pass-through: an aggregate and a limit above
a subquery with `LIMIT` or `OFFSET`, with the subquery scan kept or dropped,
targets that are not the subquery's first column, a bound from the limit
above, a correlated subquery rescanned with a changed parameter, a sort under
the subquery's limit, a clause of the scan's own, an empty result, and the
same rows with Tessera off. The [pack test](../test/tessera_pack_test.c) is a stand-in for a
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

`TessFilter` applies the clauses of a base relation to batches in the
planner's order: those the [expression compiler](expr.md) supports run as
batch filters over the function registry, the others row by row, each over
the rows the ones before it kept.
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
expected to lose until the native scan arrives (see `bench/pg/`). When
the relation may be scanned in parallel, the hook adds a partial path the
same way over the core's partial sequential scan, so that a `Gather`
above runs the node in every participant over that participant's share
of the pages: the path keeps the core scan's number of workers and rows
per participant, and is parallel-aware, as the template is, since the
node shares its counters (`EXPLAIN` prefixes it with `Parallel`); the
scan below divides the work. Without a `Gather` of its own, an aggregate
above the relation gets the core's partial aggregate over the node's rows
in each worker.

Two things make the node possible before that scan. The planner gives
every scan of the relation its clauses, so `PlanCustomPath` takes them
away from the sequential scan below the pack node, after checking they are
the ones it received, and the scan produces every row. And the columns the
clauses need but the query does not are absent from the relation's target,
so the scan below is planned with a target extended by them, while the
node keeps the relation's own target through an explicit layout that maps
each target to the child's column and hides the rest. The clauses arrive
in the planner's order and keep it: those the compiler supports go into
`custom_exprs`, the others into the plan's qualifier, and the plan data
records for each clause in turn which kind it is, so a cheap guard stays
in front of the division it protects and a security qualifier in front of
what it hides, whichever of them runs in batches. The node declares that it projects: PostgreSQL
installs a projection the query needs into the node's target list instead
of a `Result` above it, and the plan's layout is the projected one over
the child's target list as the scan tuple, derived when the plan is read.

### Execution

`BeginCustomScan` compiles the batch clauses against the child's layout,
since PostgreSQL rewrites the clauses to reference the scan tuple, which is
the child's target list, and stands on the unary helper with the clauses'
columns as its filter columns. Per batch, the expression context is reset
once and the clauses are applied through `TessQual`
([runtime.md](runtime.md)) in the recorded order, each over the rows the
previous ones left: a batch clause is bound and applied to the mask, and a
run of row-wise clauses fetches its columns with the narrowed mask, shows
each row to `ExecQual` through the scan tuple slot, and clears the bit of
a row it rejects. A batch-aware parent receives the child's slot with
the whole batch; an ordinary parent receives rows through the helper. With
computed targets the helper publishes the projection provider's wrapper of
each batch instead ([runtime.md](runtime.md)), which computes a column
when a consumer asks, for the rows asked for: a limit above narrows the
rows first. The node forwards no tuple bound, since it removes rows.

In a parallel plan the node shares only its counters: the leader lays
their rows out in the node's chunk (`TessSharedStats`,
[runtime.md](runtime.md)), a worker attaches to its own, each stores its
counters when the executor shuts the node down after the plan's last
row, and the leader shows the totals.

`EXPLAIN` shows the batch clauses as `Batch Filter` and the others as the
core's `Filter`, each in the planner's order; with `ANALYZE`, the rows removed by each part, per loop,
the helper's batches and rows and, with computed targets, the `Computed
Datums`, summed over the participants of a parallel plan. The core's
`Rows Removed by Filter` counts both parts, since the helper reports
every row the node removes.

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
relations, the planner's order in front of a division by zero, a batch
clause after a row-wise guard, a row-wise clause between batch ones, a
policy's row-wise guard before a cheaper user division, two computed
sides of a comparison and of a sum's argument, conditions of several
parts (`OR`, null and boolean tests, `IN` and `NOT IN` with a NULL, a
division behind an `OR`), conditional values (`CASE` in a clause and under
`sum`, a simple `CASE` with a text branch, `COALESCE`, `NULLIF`, a division
behind a `CASE` condition), a parallel
worker, a scrollable cursor, an `UPDATE`, and the switch off. The
parallel suite runs the node under a `Gather` with two workers (see
TessHeapScan).

## TessAgg

`TessAgg` computes the aggregates of a query without `GROUP BY` over the
batches of a batch child and returns the one result row, in place of the
core's plain `Aggregate`, which would receive the child's rows one at a
time: `TessHeapScan → TessFilter → TessAgg → parent`. It handles
`count(*)`, `count` of an expression of any type, `sum` of an int4
expression and `min` and `max` of an int4 or int8 expression over the
child's columns, computed through the projection provider: a column or
a chain the [expression compiler](expr.md) accepts, with constants and
parameters, by the chain over the batch, anything else row by row (see
"Computing columns on demand" in [runtime.md](runtime.md)); expressions
above the aggregates and `HAVING` are left to the plan's own projection
and qualifier over the aggregates. With `GROUP BY` it stands in for the
core's `HashAggregate`: each row finds the record of its keys in the hash
table of [table.md](table.md), whose payload holds the group's aggregate
states, and the groups go out in batches when the input ends.

### Planning

The module's `create_upper_paths` hook, after the hook it replaced and the
enable switch, acts on the grouping stage of the main grouping relation
for a query with aggregates and no `GROUP BY`, grouping sets or window
functions, over an input that is not known to be empty. It collects the
aggregates of the target and `HAVING` and accepts them when every one is a
plain call, without `DISTINCT`, `ORDER BY` or `FILTER`, whole or the
partial one of a parallel plan, of an aggregate the node combines and the
[function registry](function.md) implements over batches (kind
`TESS_FUNCTION_AGGREGATE`, registered by the kernels module), with an
int4 or int8 argument (any type for `count`, which reads NULL flags
alone) without a subplan whose columns, and no placeholder, the batch
child's target has. For each of the core's plain aggregate paths whose input can be read
in batches (`tess_batch_input_path`: a batch path as it is, a clause-free
sequential scan through `TessHeapScan`, anything else through `TessPack`),
the node's path takes the core path as its template at nine tenths of its
cost with the batch child, and `add_path` decides. `PlanCustomPath` makes
the distinct aggregates the scan tuple, `custom_scan_tlist` without a
relation, so that the planner turns the targets and `HAVING` into
references to it, and keeps one batch column per target. The arguments
travel in the private data with their columns resolved to the child's
targets, since `custom_exprs` would be fixed against the scan tuple of
aggregates; their parameters alone go through `custom_exprs`, so that the
planner counts them among the plan's and a node above that rescans its
child only for a changed parameter of its own, a sort in a correlated
subquery, does rescan the node.

With `GROUP BY` the hook takes the grouping expressions of the query,
1 to 16 int4 or int8 values the expression compiler accepts (a bare
column or a chain such as `c % 10`), without grouping sets, and puts them
first in the scan tuple, before the aggregates, which may then number up
to 64, since each has a flag bit in the group's payload; a grouping
without aggregates is accepted too. Every expression above the grouping
must be made of the grouping expressions, the aggregates and constants,
as the planner will rewrite it: a column the primary key makes
functionally dependent is not, and the core keeps such a query. The
templates are the core's `HashAggregate` paths, whose rows are the
planner's estimate of the groups, and there is no path when the table of
that many groups (a record of a header, a slot per key and a payload of a
flags word and a word per aggregate, with the buckets) would exceed
`hash_mem`: the node keeps every group in memory, and a table larger than
the planner expected is kept rather than split, as for `TessHashJoin`.
PostgreSQL puts a grouping expression whole into the child's target, so
a key is resolved to the child's column that holds it, which the child
computes; the plan data (`tessera.agg`, version 1) records the arguments,
the keys and the estimate of the groups.

The hook also puts the node under a `Gather`, in place of the core's
partial aggregate, when the core built partial aggregate paths for the
query: PostgreSQL calls no hook for the partially grouped relation and
builds the `Gather` and the `Finalize Aggregate` before it calls this
one, so the hook finds that relation itself and, for each of its partial
aggregate paths, builds the whole stack: the node's partial path over the
batch child of the core path's input (a partial `TessFilter`, a
clause-free scan through `TessHeapScan`, anything else through
`TessPack`), with the partial aggregates of that relation's target as its
scan tuple, the core's `Gather` over it and the core's
`Finalize Aggregate` over that, which combines the participants' values
with the aggregates' combine functions and applies `HAVING`; `add_path`
decides against the core's stack and the node's serial path. The partial
path is parallel-aware for the counters the node shares, and the plan's
qualifier is empty, since `HAVING` belongs to the `Finalize Aggregate`.
The partial values are the whole ones' types, int8 for `count` and
`sum`, the argument's for `min` and `max`, so the node computes them as it
computes the whole ones, and a participant without rows gives a count of 0 and
NULL otherwise, which the strict combine functions skip. With `GROUP BY`
the stack is the same over the core's partial hashed aggregate paths:
each participant keeps a table of its own groups, and the core's
`Finalize HashAggregate` over the `Gather` merges them with the
combine functions and applies `HAVING`; its estimate of the groups is
taken from the core's grouped paths, since the grouped relation's rows
are set only after the hook.

### Execution

`BeginCustomScan` creates a projection over the child's target list with
the arguments as its computed columns, finds each aggregate's batch
function, asks the child for whole batches with the arguments' columns as
projection columns, so that a lazy provider deforms them for the surviving
rows only, and stands on the output helper over the result slot with a
one-row builder. The first execution reads every batch of the child: the
batch is wrapped, each argument's computed column, a chain over the
batch's rows or the executor's expression row by row, and the batch's
rows go to the batch function, the wrapper is released, and the partial
the function returns joins the running value, by
int8 addition checked for overflow (`bigint out of range`, as the core's
`int8inc`; the core's `int4_sum` does not check, which differs only past
four billion rows) for `count` and `sum`, by comparison, as int4 or int8
after the aggregate's transition type, for `min` and `max`; `sum`, `min` and `max` stay NULL without a contributing row, `count`
is 0. The row is built in the scan slot, `HAVING` is evaluated over it, the
plan's projection runs when the targets are not the bare aggregates, and
the row is published as a one-row batch: a batch-aware parent such as
`TessLimit` reads the batch, an ordinary parent the row. The next call
returns nothing.

With `GROUP BY` the keys are the projection's first computed columns and
the table lives in a memory context of its own, created at the first
execution for the planner's estimate of the groups (256 at least). Per
batch the keys are hashed in key order with NULL as a key of its own
(`TESS_NULL_KEYS_GROUP`), so rows with NULL keys form one group, and
`tess_table_find_or_insert` gives each row the record of its group,
creating it with a zero payload; the rows left pending when the table is
full go in after the region doubles (`repalloc` and `tess_table_grow`).
Each aggregate then folds the batch into the records' states with
`tess_table_accumulate`, one call per aggregate and batch: `count(*)`
and `count(x)` add one, `sum`, `min` and `max` take the non-NULL values
and set the aggregate's flag bit, so a group without a value stays NULL.
When the input ends, `tess_table_scan` walks the groups 64 at a time,
`tess_table_gather_key` and `tess_table_gather` bring their keys and
states, and each group is built in the scan slot, `HAVING` is evaluated
over it and the plan's projection runs, the rows that pass going into a
batch of up to 64 rows: a batch-aware parent reads one batch per call, a
row-wise one the rows one by one; a batch `HAVING` leaves empty is not
published. The order of the groups is the table's insertion order and is
not promised, as for the core's `HashAggregate`. A rescan builds the
table again from the rescanned child. Rescan passes changed parameters on to the child, since
the core does that for outer and inner plans only, and resets the values.
The node forwards no tuple bound, as the core's aggregate does not. Under
a `Gather` every participant runs the node over its share of the child's
batches and sends its one row of partial values up the tuple queue; the
node shares only its counters (`TessSharedStats`,
[runtime.md](runtime.md)): the leader lays their rows out in the node's
chunk, a worker attaches to its own, each stores its counters when the
executor shuts the node down after the plan's last row, and the leader
shows the totals.

`EXPLAIN` shows the grouping expressions as the core's `Group Key`,
`HAVING` as the core's `Filter` and `Partial Mode: Partial` under a
`Gather`; with `ANALYZE`, the batches and rows read from the child, the
batch function calls (per aggregate and batch with `GROUP BY`), the
`Computed Datums` of the keys and the arguments, by chains and row by row
together, and with `GROUP BY` the groups, the times the table grew, its
`Memory Usage` and `Overrun`, what of it exceeded `hash_mem` (shown only
then), summed over the participants of a parallel plan.

### Tests

`test/sql/agg.sql` compares the results of every query with Tessera on and
off through one function: the node above the native scan, above the
filter, above pack over a filtered scan and above the hash join; the same
aggregate twice, expressions above the aggregates, `HAVING` true and
false; the four column aggregates with `count(*)` over a filtered table,
the whole table, nothing and a column of NULLs; chains and a parameter
in the argument with a generic plan re-executed; a correlated subquery,
also with a hash aggregate under pack; a limit above reading the node's
batch; a scrollable cursor; a single-copy `Gather`; the overflow of a
chain; `count` of a text column; a bigint column with `min`, `max` and
`count`, the filter and the argument chains through the mixed operators
of bigint with an integer constant, the cast of an int4 column as an
argument, the bigint extremes under a single-copy `Gather` and the
overflow of a bigint chain; and the core keeping `DISTINCT` and `FILTER`
in the aggregate, a window function, an empty relation,
`sum` over bigint, `avg`, the switch off and the kernels module absent.
With `GROUP BY`: a bare key and an expression key (computed by the scan),
NULL keys as one group, two keys, expressions over the keys and the
aggregates, `HAVING` over a filter, grouping without aggregates, a
constant target, an empty input, one group, int8 keys and extremes, a
grouping over the hash join and in a rescanned subquery, 100 000 groups
against an estimate of 200 (the table grows), a sort above reading the
groups row by row; and the core keeping grouping sets, a text key, a
functionally dependent column and a disabled hash aggregation. The parallel
suite (`test/sql/parallel.sql`) runs the node under a `Gather` with two
workers: the five aggregates with and without a clause, chains and
row-wise arguments, expressions above, `HAVING` true and false,
participants without rows, the leader not taking part, a generic plan's
parameter in an argument, the `Gather` rescanned in a join, and an
aggregate the node leaves to the core's partial aggregate; and groups
under the `Gather` with a filter, an expression key with `HAVING`, the
leader not taking part.

## TessHashJoin

`TessHashJoin` joins two batch children on equalities of integer keys,
in place of the core's `Hash Join`: it builds the rows of the inner child
into the hash table of [table.md](table.md) and probes it with the
batches of the outer child, so that neither side is handed over one row
at a time and a batch parent such as `TessAgg` reads the joined rows as
batches. The table and the key hashes are the Rust kernels', which the
node calls through the bridge's kernel registry (see
[bridge.md](bridge.md)): the node module links no Rust, and without the
`tessera_kernels` module there is no path.

### Planning

The module's `set_join_pathlist` hook offers the path for an inner, a
semi, an anti or a left join, the kinds that keep the outer side, which
the node probes with (a right or full join would need marks on the
table's records, and the hook passes the right join the other way round
over), with at least one `int4eq`, `int8eq`, `int48eq` or `int84eq` between a
column of each side, when the join's target is plain columns and at most
64 of them, the inner keys and the inner columns of the residual clauses
are the inner side's. Each such clause, up to 16, is a key of the table;
the others are residual clauses, evaluated over the joined rows, when
they read plain columns only (no placeholder) and are not
pseudoconstant, in the order the core's hash join evaluates them. An
outer join, left or anti, splits its clauses as the core does: a clause
pushed down to the join from above (a `WHERE` clause over a left join)
is no join clause and never a key but a filter over the rows the join
returns, after NULL extension, in the same order. Above a left join an
inner column is marked as nulled by it while the join's own clauses
read it unmarked, and the planner matches every expression of the plan
against the scan tuple's one entry of its column, marks included: the
clauses' columns take the entry's marks, which the executor never
reads. `order_qual_clauses` is private, so the hook sorts the clauses with the same
key as for `TessFilter`, and a guard runs before the division it
protects whichever of them runs in batches; keys go into it as 8-byte values and
an int8 inside the int4 range hashes as the int4, so every combination
of the two types uses one table. The children are batch
paths over the sides' cheapest paths (`tess_batch_input_path`: a native
scan, a batch path as it is, or pack over anything else). The hook is
called for both orders of the sides, and as in the core the inner side is
the one built, so the cost decides which side that is. The template is
the core's hash join of the same inputs (`initial_cost_hashjoin` and
`create_hashjoin_path`, not added), at nine tenths of its cost; its
disabled count comes along, and like the core the hook offers nothing
when hash joins are disabled. The node keeps the whole table in memory
and does not split it into batches, so there is no path when the core
would split the inner side, or when the node's own estimate of the table
(a record of 24 bytes, a word per key and per inner column, the
columns' width, the buckets) exceeds `hash_mem`.

Under a `Gather` the hook also offers a partial path: the outer side's
cheapest partial path divides the rows, and every participant builds the
whole inner side from the cheapest inner path a worker may run, as the
core's hash join without a shared table does; the template is that
join's cost. The core's parallel hash join, whose shared table divides
the build, usually costs less; a shared table of the node's own is item
5.5 of the plan. The partial path is parallel-aware only for the
counters the participants share through `TessSharedStats`.

The plan's scan tuple is the join's columns, the outer side's first, and
the keys of both sides and the residual clauses' columns, which the
clauses in `custom_exprs`, the keys' first and the residual ones after
them, refer to. The path supports projection
(`CUSTOMPATH_SUPPORT_PROJECTION`): a target above the join becomes the
node's own, with no `Result` between the node and a batch parent. A
target that is a column of the scan tuple maps to it, whatever the
order, and an expression is a computed column, which `TessProjection`
computes over the pairs, by chains where it can and row by row
otherwise; the scan tuple takes its columns from the join's target,
since PostgreSQL plans a projecting node without a target list
(`TESS_LAYOUT_PROJECTED`). The
plan data records each column's side and its column in that child's
batches, each key's column and kind on each side, for each residual
clause in turn whether it runs in batches, whether the inner side is unique and
the planner's estimate of its rows.

### Execution

At its first execution the node derives both children's requests from
its parent's: the outer columns asked for come from the outer batches,
the inner ones are kept in the table, and each side gives its keys
first. It then reads every inner batch, hashes the keys in order, each
further key folded into the first one's hash, with the NULL policy of a
join (a NULL in any key never matches) and inserts the rows with
`tess_table_insert_grouped`, which puts a key's records next to each
other in their chain and reports the rows whose key was there already;
the node counts them. A record's payload is a word of the NULL bits of
the kept inner columns and a Datum per column; a by-reference value is
copied into the node's memory, where it lives as long as the table. The
node also notes which kept columns hold a NULL at all. The table starts at the planner's
estimate of the inner rows; when it is full, `repalloc` doubles the
region, the table rebuilds its buckets and the rows left pending go in.
An empty inner side ends the scan without reading the outer child.

Each outer batch is then hashed and probed. The rows that found a record
are a round: the node publishes its own batch with the outer batch's
physical rows and the round's rows selected. An outer column of that
batch is the outer batch's own column, passed through without a copy; an
inner column is gathered from the round's records with
`tess_table_gather` when a parent first asks for it, and the records'
NULL bits once per round, only when a column asked for holds a NULL
somewhere in the table. A key held by several inner rows has as many
records, and `tess_table_next_in_group` gives the next round in one step
per row from the node's own copy of the round's rows, since a parent may
narrow the published mask. The residual clauses are applied through
`TessQual` (see [runtime.md](runtime.md)), those the expression compiler
takes, such as `f.f1 > d.d1 * 10`, a column against a chain over the
other side, in batches and the others row by row, in their order, to each round or
compact batch before it is published: they narrow the published
selection only, the round's own rows staying whole for the next round,
and a batch they leave empty is skipped. There is no second round when the planner
knows the inner side unique or the build met no duplicate key: walking a
chain to find that a key has no other record cost as much as the probe
itself.

A batch-aware parent over a table with duplicate keys gets compact
batches instead: the node copies the pairs of the rounds one after
another into batches of 64 rows, each outer column's value copied from
the round's outer batch and each pair's record kept, and gathers the
inner columns from those records. Published over the outer batch, a key
with four records would give four batches with a quarter of their rows
selected, each paying the whole cost of a batch in the parent. A
by-reference value points into its outer batch, which goes before the
compact batch does, so it is copied into a memory context the node
resets before it fills the next compact batch, once the parent has
released the previous one. A round with at least half its rows selected,
met while nothing is copied yet, goes out as it is: copying it would
only cost, and a by-reference value the most. Without duplicates a round
is dense enough that copying the outer columns costs more than it
saves. The outer batch stays active until its
last round is finished. A row-wise parent is served from the round's
columns row by row. The node scans forward only: a scrollable cursor
gets a `Material` above it. A rescan builds the table again only when
the inner child has changed parameters, as the core's hash join decides,
and otherwise probes the same table with the rescanned outer child.

A semi or anti join marks the rows of each outer batch that have a pair
passing the join clauses: without such clauses the rows the probe found,
with them the rows of each round that pass, which then leave the next
rounds, since one pair decides. The batch then goes out once, with the
marked rows for a semi join and the others for an anti join, a row with
a NULL key having no pair; an anti join's filters apply to it. A left
join publishes the rounds of pairs as an inner join does, marking the
rows whose pairs pass the join clauses, and then a round of the rows
without one, over the outer batch, whose inner columns are all NULL; the
filters apply to every batch it returns. In compact mode the round of
unmatched rows goes out alone, after the compact batch of pairs in
progress; a left join with join clauses stays out of compact mode, which
would copy the pairs before the clauses mark the rows that have one, and
semi and anti joins, which return outer rows, never enter it. With an
empty inner side the outer child is still read for a left or anti join,
whose rows all go out.

`EXPLAIN` shows the join type for a semi, anti or left join, the key
clauses as `Hash Cond`, the residual ones that run in batches as `Batch
Join Filter` and the others as `Join Filter`, and an outer join's filters
as `Batch Filter` and `Filter`. With `ANALYZE` it adds
the bucket count of the last table built, `Memory Usage`, the most the
table and the copies of inner values took, `Overrun`, what of it
exceeded `hash_mem` (shown only then: the node keeps the whole inner side
in memory, and a table larger than the planner expected is kept rather
than split), `Builds`, the tables built over the rescans, `Build Rows`,
the inner rows inserted into them, `Table Grows`, the doublings of the
region, `Probe Rows`, the outer rows probed, and `Matches`, the joined
rows over every round, `Rows Removed by Join Filter` and `Rows Removed by
Filter`, and `Compact Batches`, the batches of copied
pairs, when there are any. Under a `Gather` the counters are the totals of
every participant, and the bucket count is the mean over the tables
built.

### Tests

`test/sql/join.sql` compares the rows of every join with Tessera on and
off through one function: counts and sums under `TessAgg`, which reads
the join's batches without pack; rows to the client with columns of both
sides, NULLs and text of the inner side; the keys as targets and no
target at all; int8 keys past the int4 range and an int4 key against an
int8 one both ways; two keys, with NULLs in the second, an int8 key next
to an int4 one, three keys, and composite keys with duplicates under an
aggregate and as rows; targets above the join in another order than the
join's and expressions over both sides, as rows, under a sort, with
rounds and a residual clause; a row-wise guard over both sides before a
batch division, in either written order; an `OR` with a null test over
both sides; a `CASE` over both sides under `sum`; residual clauses over int4 columns of both sides
with NULLs, text, an OR over both sides, with rounds and compact
batches, a text equality next to the key and a parameter of an outer
query; three inner rows per key, duplicates on both sides
and NULL keys on both; compact batches under an aggregate, with NULLs in
an outer column, and with text outer columns under an aggregate and a
limit; a side
of fewer than 64 rows, an empty side on
either side, a join over a join; an inner side much larger than the
planner's estimate, which makes the table grow; a top-N sort and a limit
above the node, and a scrollable cursor through `Material`. With
`EXPLAIN ANALYZE`, the memory masked, it shows the counters of a join, of
one with rounds and of an inner side of 20000 rows estimated at 10, which grows
past a small `work_mem`; correlated subqueries whose parameter is on the
inner side, which builds the table for every outer row, and on the outer
side, which builds it once; and a generic plan executed with two
parameters. Under a `Gather` with two workers, the core's shared hash
table disabled, it compares an aggregate over the node's partial path,
rows through the `Gather`, rounds, and the leader not taking part, and
checks that the rows probed and the matches are the totals of every
participant. It also shows
the core's plan without the kernels module, for a full join, clauses
without an integer key, a text key, hash joins disabled and the switch off.
Semi, anti and left joins: `EXISTS` with and without a join clause, `IN`
over a subquery, `NOT EXISTS` with and without one (NULL keys going
out), a left join the planner turns into an anti join, left joins with
misses and NULL keys, with duplicates as rows and in compact mode under
an aggregate, with a join clause in `ON` and a filter in `WHERE`, under a
sort; an empty inner side for each kind; rescans with a parameter in the
join clauses; and under the `Gather` a left, a semi and an anti join.
