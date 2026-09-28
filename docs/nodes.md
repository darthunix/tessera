# Nodes module

`tessera_nodes` (`nodes/`) is the module of Tessera's own batch nodes. It
links the runtime library statically and, when loaded, registers its node
kinds in the bridge's node registry and its scan methods with PostgreSQL.
The pack node below is created by batch parents and needs no hook; the
filter node offers its path to base relations through the module's
`set_rel_pathlist` hook, which also offers the heap scan node's path for
a relation without clauses, the aggregate node to the grouping stage
through its `create_upper_paths` hook, the hash join node to joins
through its `set_join_pathlist` hook, the sort node to the ordered stage
through another `create_upper_paths` hook, the gather node to the final
stage through a third. The append node, like the pack node, is created by
batch parents. It is loaded after the bridge; loading it without
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
limit's child, so an ordinary scan gets a pack node below. Over any other
row-wise child (a sort, an aggregate, a join) the core limit stays: a pack
there would copy every row the limit reads. It stands on
the unary helper, removes the offset's rows and the rows past the count,
stops the input once the count is reached, and passes the tuple bound to
the child, through the pack node when there is one.
`WITH TIES` and parameterized inputs stay with the core node. Load it
after the bridge, and after the nodes module when ordinary children should
be packed; without a pack node it adds no paths. The
[node-writing guide](writing-a-node.md) walks through it.

## TessHeapScan

`TessHeapScan` reads a plain heap table in batches, in place of the pack
node over the core's sequential scan, which returned every row through
`ExecProcNode` into a slot, for a batch-aware parent, and in place of the
core's sequential scan itself for any other. The chain of the first
queries is therefore `TessHeapScan → TessFilter → parent`.

### Planning

The node publishes `scan_rows`: a path over a sequential scan of a plain
heap table (`RELKIND_RELATION`, the heap access method, no sampling; a
partition or an inheritance child, not the parent that has them) whose
targets are its columns or expressions over them. `tess_batch_scan_path` builds it
for a parent that evaluates the relation's clauses itself, as the filter
does, and `tess_batch_input_path` prefers it to the pack node for a
relation without clauses. For such a relation the module's
`set_rel_pathlist` hook also adds it to the relation's paths next to the
core's sequential scan, at the filter's nine tenths of its cost, and a
partial path next to the parallel one: the node is faster than the core's
scan under a row-wise parent too (an aggregate the node does not compute
over 2 M rows took 24.7 ms against 33.1, one column of sixty 5.3 against
11.9, `bench/pg/rowwise.sql`), since it pins a page once rather than per
row and deforms only the columns read. So a relation read under any
parent gets it, a subquery planned apart from a batch parent above it,
such as a branch of `UNION`, among them. A pseudoconstant clause keeps both helpers
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

The parent's request is frozen at the first execution. A row-wise parent
gets the rows of each batch one per call: the columns of the node's
targets are taken from the batch once, for all its rows, deformed or
computed, and each call copies a row's values into the node's slot; the
batch, and its pins, go when the next is read. The scan is begun with the query's snapshot: in a
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
from older tuples, and a clause the planner folds away; and, for a
row-wise parent, an aggregate the node does not compute, rows to the
client with computed targets, NULL and text, a sort and a window function
of the core, a cursor fetching in parts, a rescan per outer row with a
parameter among the targets, and several pages with dead tuples and
values stored outside the page. The parallel
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
enable switch, considers a plain `SELECT` over one heap table, or a
partition or inheritance child with the clauses the core translated from
its parent, without sampling, row marks or lateral references, with at least one
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

A relation whose clauses all run row by row (`LIKE`, a text comparison,
the plain clauses without the kernels module) gets the node's path too,
over the native scan, at the same nine tenths of the sequential scan's
cost, and a partial path next to the parallel one
(`tess_filter_row_path`): the node's rows cost less than the core scan's
under any parent, a row-wise one included (`bench/pg/rowwise.sql`:
`bit_or` over `LIKE` 9.4 ms against 11.2), and a batch parent above reads
its batches instead of a pack's copies. An inner or semi hash join above
it hands it its Bloom filter, which then reaches the rows before the
row-wise clauses (plan item 5.2). Such a node shows no `Batch Filter`.

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
a row it rejects. A hash join above may hand the node its Bloom filter
(`set_key_filter`, [node.md](node.md)) when the node has a row-wise
clause and the join's keys are columns of the child: the node then
hashes the keys of the rows its batch clauses kept, as the join does,
and removes the rows the filter rejects before its first row-wise
clause, a shared filter once it reads it ready. A batch-aware parent receives the child's slot with
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
Datums`, summed over the participants of a parallel plan, and `Rows
Removed by Bloom Filter`, the rows a join's filter removed, when there
are any. The core's
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
and qualifier over the aggregates. Any other aggregate without `GROUP BY`
(of text, numeric, float8, int8 states, of several arguments such as
`string_agg(t, ',')` or `corr(x, y)`, of polymorphic ones such as
`array_agg`) goes through the core's own functions: its transition
function is called for each selected row of the arguments' columns of a
batch, as the core's `Aggregate` calls it per row but without a row
handed up, then its final function, or, in a partial plan, its
serialization function for the core's Finalize Aggregate (`bench/pg/
anyagg.sql`: ×0.36–0.86 against the core). With `GROUP BY` it stands in for the
core's `HashAggregate`: each row finds the record of its keys in the hash
table of [table.md](table.md), whose payload holds the group's aggregate
states, and the groups go out in batches when the input ends.

`SELECT DISTINCT` is grouping without aggregates: at the distinct stage
the node's path stands next to each of the core's hashed distinct paths,
its keys the distinct expressions (`DISTINCT ON` stays with the core).
An aggregate with `DISTINCT` over an int4 or int8 argument keeps a table
of its own, without payload, keyed by the group's keys and the argument
(the argument alone without `GROUP BY`): a batch's rows go into the
aggregate only where they inserted their pair, NULL arguments dropped by
the hash. The core groups such a query only sorted; the node takes its
`GroupAggregate` or plain `Aggregate` as a template and reads the input
below the core's sort, unless `enable_hashagg` is off (as for any grouping
whose hashed path the core dropped). The pairs' tables
do not spill, and neither do the groups of a query that has them: the
node takes the path only when the planner's estimate of the pairs fits
`hash_mem`, and shows their bytes in `Memory Usage`.

`UNION` without `ALL` is grouping of the branches' rows by every column:
at the set operation stage (`UPPERREL_SETOP`) the node's path stands next
to each of the core's hashed aggregates over the `Append` of the
branches, its keys the columns, 1 to 16 of them, of the types a key's
word holds (see `GROUP BY` below), and its
child `TessAppend` over the branches' batch paths. Only the set
operation of the whole query (of a subquery, when it is one) gets it:
above that the core puts only a sort and a limit, which read columns by
position, while a set operation within another could have a projection
above it that looks for the set operation's own columns, which the
node's plan shows as its first branch's (see "Building paths" in
[runtime.md](runtime.md)). The groups spill as those of `GROUP BY` do.

`INTERSECT` and `EXCEPT`, with `ALL` or not, are grouping of both sides'
rows by every column too: the node's path stands next to each of the
core's `SetOp` paths of the whole query, hashed or sorted, with two batch
children, the sides' paths below any sort, and keys of any type the
grouping takes (words, or values through a dictionary). The node reads
the left side, then the right, each through an input, a projection and a
layout of its own, the keys the sides' columns by position; the scan
tuple adds two aggregates the plan makes, `count(*)` and `sum` over the
side, a constant each side's projection computes, 0 for the left and 1
for the right, so that a spilled row keeps its side. After the input a
group goes out, as its first key values, `EXCEPT` once when only the left
side has rows, `EXCEPT ALL` as many times as the left side's rows exceed
the right's, `INTERSECT` once when both have rows, `INTERSECT ALL` as many
times as the fewer; copies that do not fit a batch go on into the next,
and a batch goes out before the walk moves to the next groups, whose
reading may free the values the batch points into. While no group has
spilled and the table is not frozen, the right side's rows only find
their groups (`table_probe`, the dictionary looked up without numbering
new values) and a row of no group is dropped, as the core's `SetOp` does:
right-side groups cost 15 % of an `INTERSECT` of text. The groups spill
past `hash_mem` as those of `GROUP BY` do, where the core's hashed `SetOp`
would not be chosen. Of equal values of different forms (numeric of other
scales, text under a case-insensitive collation) a group can go out with
another form than the core's: a dictionary keeps a value's first form of
the whole input, the core the group's first row's. A dictionary starts
with room for the planner's estimate of the groups, within a quarter of
`hash_mem`. The cost is the sides' batch paths and a share of the
core's `SetOp` over its own inputs, 0.5 with keys of words and 0.9 with a
key through a dictionary, as measured: 500 000 integers `EXCEPT` a third
of them 23.9 ms against 57.8, `EXCEPT ALL` of 1000 values 34.9 against
95.2, `INTERSECT ALL` 60.1 against 170.5, `INTERSECT` of texts 80.6
against 89.7 (pg-setop-6kROdR).

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
alone), or, without `GROUP BY` and `DISTINCT`, of any other ordinary
aggregate of one argument or more, through the core's functions, without
a subplan whose columns, and no placeholder, the batch child's target
has. For such an aggregate `BeginCustomScan` reads `pg_aggregate` as the
core's `ExecInitAgg` does: the transition function with the call
expression a polymorphic one asks its argument types of, the final
function with its extra arguments, or the serialization function of a
partial plan, and the initial value. The states live in a context of the
node's, which a stand-in `AggState` hands the transition functions that
ask for the aggregate's memory (`AggCheckCallContext`); a strict
function skips a row with a NULL argument and, without an initial value,
takes the first kept argument as the state; a new by-reference state is
copied into that context and the old one freed, and what a call
allocates besides goes with the batch's memory. The arguments after the
first travel in the private data as `more`. With `GROUP BY` such an
aggregate's state is a word of the group's record, the value itself when
a word holds it, else the address of its copy in that context, with the
aggregate's flag bit set while it is not NULL: the groups a batch
inserts start from the initial value, and each row, in order, since rows
of one group may follow one another, reads its group's state from the
record, advances it and writes it back; a group's final value is
computed when the group goes out, in memory reset per group. Such groups
cannot spill as records (they hold addresses), so past seven eighths of
`hash_mem`, counting the states' memory, they go the core's way: the
table freezes and takes no new group, the rows of its groups go on into
them, and the rows of the groups it lacks go, the values of every key
and argument (`TESS_SPILL_COLUMNS` blocks of 256 rows and blocks of
their by-reference values), to 32 partitions on disk by five bits of
their hash. Once the groups in memory are out, each partition is read
back, 64 rows a batch, into a table of its own with fresh states; one
that does not fit either spills by the next five bits, depth first, six
levels at most (`Spilled Rows` in `EXPLAIN ANALYZE`). A group's rows stay
in their order, as an order-sensitive aggregate such as `string_agg`
needs. With a `DISTINCT` aggregate alongside, whose table does not
spill, the path is taken only when the planner's estimate of the groups
and their states, a type's average width or an internal state's
declared space or 1 kB as the core estimates them, fits `hash_mem`. Not
in a partial plan, whose table empties early. For each of the core's plain aggregate paths whose input can be read
in batches (`tess_batch_input_path`: a batch path as it is, a clause-free
sequential scan through `TessHeapScan`, anything else through `TessPack`),
the node's path takes the core path as its template with the batch child,
and `add_path` decides. A plain aggregate costs nine tenths of the core's.
A grouping costs the node's own (`group_cost`): the child's cost; per
input row a quarter of `cpu_operator_cost` a key, as the kernels hash and
look up a batch's keys at once, and the aggregates' transition costs as
the core counts them (`get_agg_clause_costs`), a quarter of them when
the node's kernels fold every aggregate; per group `cpu_tuple_cost` and
the final costs; and, when the groups at the core's bytes per entry pass
seven eighths of `hash_mem`, the rows of those that do not fit written
to 32 partitions and read back once per level, sequentially, with
`cpu_tuple_cost` a row, without the core's penalty for random writes.
When the core's sorted grouping has beaten its hashed one out of the
relation's paths (its spill costs more), the node takes the sorted path
as its template and reads the input below the sort. `PlanCustomPath` makes
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
1 to 16 values of a type the table keeps in a word, whole and compared
bit for bit, as PostgreSQL's Datum of the type holds it: int2, int4,
date and bool, sign-extended as int4 keys, int8, timestamp and
timestamptz as int8 keys (`tess_word_key_kind`), or of any other type
whose equality hashes (text, numeric, ...): such a key's values get
numbers through a dictionary of the node's, a `simplehash` table that
hashes and compares them by the type's own functions (the key's
equality and its hash function, under the key's collation, so a
nondeterministic collation groups as the core's does), in the order
the table first meets them, and the table groups by the numbers as
int8 keys; a group's key goes out as the value of its number. Where equal
values are equal bytes and the type's hash function hashes those bytes
(text and varchar under a deterministic collation, `texteq` and
`hashtext`; bytea, `byteaeq` and `hashvarlena`), a value neither
compressed nor external is hashed with `hash_bytes` and compared by its
length and `memcmp` in the dictionary's loop, as those functions would,
without a call through fmgr, which looked the collation up on every
call; a compressed or external value takes the functions, and hashes
alike. Grouping 500 000 rows of text by 99 values took 15 % less, by
450 000 values 4 % less, `INTERSECT` of texts 10 to 13 % less
(pg-anykey-2vdcGC, pg-setop-tzlEdr against the build before). A
dictionary past the caches (its values' entries past 1 MB, counted by
the values held, not by the buckets, which a table made for the
planner's estimate may have many more of) is looked up a batch at a
time: a pass of its own asks memory for each row's bucket before the
lookups, so that the batch's misses overlap. The values' copies go one
after another into blocks of 64 kB of the dictionary's, without a
chunk's header each (a value past 16 kB, or an expanded object, is a
copy of its own), and an entry is 24 bytes, its number 32 bits. Grouping
by 450 000 values of text took 13 % less, `INTERSECT` of texts 8 % less,
small dictionaries 1 to 3 % less (pg-anykey-OqakjS, pg-setop-UmZbY2
against the build before). Both parts matter: the pass alone took 2 to
3 % off, the blocks and entries alone nothing; hashes kept from the pass
for the lookups, or a second copy of their loop, made the compiler stop
inlining them and cost small dictionaries 3 to 4 %. The table fills to
three quarters, not simplehash's nine tenths, near which its robin hood
runs grow long: a dictionary of 450 000 values made for an estimate of as
many, 86 % full, took 17 % more of the grouping, `INTERSECT` of texts 12 %
more; small dictionaries 1 % less (pg-anykey-vw7dd9, pg-setop-lHfxx5
against the build before; half the fill did no better). The
dictionary is the table's: made anew with it, and its memory counted
with the groups', so such a grouping spills its rows, with the values,
never its records, and a spilled row's partition is chosen by the hash
of its values, not of its numbers (with numbers, every row of a group
the frozen table lacks would take one partition). A key is a bare column, a chain the
expression compiler accepts such as `c % 10`, or any other expression,
computed row by row, without grouping sets, and the hook puts them
first in the scan tuple, before the aggregates, which may then number up
to 64, since each has a flag bit in the group's payload; a grouping
without aggregates is accepted too. Every expression above the grouping
must be made of the grouping expressions, the aggregates and constants,
as the planner will rewrite it: a column the primary key makes
functionally dependent is not, and the core keeps such a query. The
templates are the core's `HashAggregate` paths, whose rows are the
planner's estimate of the groups, and their cost counts the batches the
core would write; the node spills where the core's would (see Spilling
below).
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
each participant keeps a table of its own groups, sent up early when it
fills and folds, and written to disk as a serial node's when it does
not (see [spill.md](spill.md), "Partial mode"), and the core's
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
the table lives in a memory context of its own, its index created at the
first execution for the planner's estimate of the groups (256 at least,
and at most what a quarter of `hash_mem` holds) and its records in
chunks, the first of 64 kB and the others of an eighth of `hash_mem` up
to 1 MB. Per
batch the keys are hashed in key order with NULL as a key of its own
(`TESS_NULL_KEYS_GROUP`), so rows with NULL keys form one group, and
`tess_table_find_or_insert` gives each row the record of its group,
creating it with a zero payload in the last chunk; the rows left pending
go into another chunk when that one is full, or, when the groups reached
half the buckets, after `tess_table_regrow` made an index for twice the
groups over the same chunks, the records staying where they are.
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
published. When the result is the scan tuple itself (no `HAVING`, no
projection, no generic aggregate, whose values are made one group at a
time) the groups go out as the node's own batch instead: its columns are
the walk's arrays of keys (a dictionary number already its value, which
the dictionary keeps until the next partition's rows are read, after the
batch is done with) and of the aggregates' values, without a row built
or a value copied; the rows of the builder copied every by-reference
value twice, into the result slot and into the builder, and took 15 % of
a grouping of 450 000 texts: `any_text_col` (450 000 groups of text)
took 15 % less, `setop_many` (`UNION` of 2 M integers) 16 % less, the
other cases of anykey, setop, win and wordkey the same within 3 %
(pg-anykey-ro4VMq, pg-setop-HkmtA4 against the build before). The order of
the groups is the table's insertion order and is
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
`Memory Usage`, and for a table that spilled `Batches`, the most
partitions of a level, `Evictions`, the partitions sent to disk while
the input was read, `Spilled Chunks` and `Disk Usage`, the chunks and
bytes written, and `Split Partitions`, those split into a level below,
summed over the participants of a parallel plan.

### Spilling

The groups spill once the table would outgrow `hash_mem` (see
[spill.md](spill.md)). What goes to disk is not the input rows but the
groups' records, each a group's keys and states, and a group has no
by-reference value, so no value chunks are needed. Past seven eighths of
`hash_mem`, the eighth left for a batch's new chunk and index, the node
chooses a power of two of partitions by the hashes' low bits, splits
the records so far into them (`tess_table_split`) and makes one index
over them all; new groups go to their partitions' chunks
(`tess_table_find_or_insert_partitioned`), a partition without room
taking another. While the table, its files' buffers included, takes more
than seven eighths of `hash_mem`, the partition with the most bytes in
memory goes to disk whole, its chunks written and freed, and the index
is made anew over the rest. Its rows go on making new records, which fold
the rows of its hot groups in memory until the next time it goes; a
group's states may thus lie in several records, one per time its
partition went to disk.

When the input ends, the partitions are given out in turn. A partition's
records in memory are linked into a table of their own, each group once,
and its chunks read back merge into it (`tess_table_combine`: counts and
sums add, minima and maxima keep the extreme); its groups then go out as
before. A partition whose groups would not fit, by an estimate of them
(HyperLogLog over its records' hashes, see [spill.md](spill.md)) rather
than by its records on disk, of which a group evicted many times has
many, splits first by the next bits of the hash into a level of its own, all its records written again
by partition, and the level's partitions are merged in turn, their
chunks in memory merged as sources too, since a split does not find a
group's other records; a level given out hands back to the one above.
The chunks live in memory contexts of small blocks, so that a chunk
takes a block of its own size and memory is what the chunks take.

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
against an estimate of 200 (the index is made anew), a sort above reading the
groups row by row; spilling at a `work_mem` of 1 MB, 200 000 groups of three rows
with NULL keys and values, with the counters, every group compared by an md5 of
them all for the four aggregates, two keys and `HAVING`, ten hot groups among
the rare ones, no split where each partition's groups fit however often they
went to disk, 200 000 groups the planner expects 10 of (a level below), a sort
above reading the groups row by row and a rescan with a parameter; and the core keeping grouping sets, a text key, a
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

`TessHashJoin` joins two batch children on equalities of keys of any
hashable type, in place of the core's `Hash Join`: it builds the rows of the inner child
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
the node probes with, and for a right or a full join, which keep the
inner side too (see "Right and full joins" below), with at least one
equality that compares words bit for bit between a column of each side:
of integers (int2, int4, int8) in any combination, of two dates, two
timestamps, two timestamps with time zone or two booleans (a date against
a timestamp converts, and stays with the core), or, for keys a word does
not hold (text, numeric, ...), the type's default equality, which has a
64-bit hash function (`hash_extended_proc` of the type cache), between
two columns of that type, when the join's target is plain columns and at most
64 of them, the inner keys and the inner columns of the residual clauses
are the inner side's. Each such clause, up to 16, is a key of the table;
a key a word does not hold is its value's 64-bit hash by that function
under the clause's collation (seed 0), kept in the table as an int8, and
its equality is also a residual clause, which decides whether two rows
of one hash are a pair: the kernels pick candidates by the hashes, and a
false one is no pair, for every join kind, spilling and the shared table
alike. The others are residual clauses, evaluated over the joined rows, when
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
scan, a batch path as it is, or pack over anything else); for an inner or
semi join an outer relation whose clauses all run row by row is read
through TessFilter instead (see TessFilter, Planning), where the join's
Bloom filter reaches it. The hook is
called for both orders of the sides, and as in the core the inner side is
the one built, so the cost decides which side that is. The template is
the core's hash join of the same inputs (`initial_cost_hashjoin` and
`create_hashjoin_path`, not added), at nine tenths of its cost; its
disabled count comes along, and like the core the hook offers nothing
when hash joins are disabled. The template counts the batches the core
would write, and the node's table spills where the core's would (see
Spilling below), a shared one past every participant's `hash_mem`.

Under a `Gather` the hook also offers a partial path: the outer side's
cheapest partial path divides the rows, and every participant builds the
whole inner side from the cheapest inner path a worker may run, as the
core's hash join without a shared table does; the template is that
join's cost. The partial path is parallel-aware for the counters the
participants share through `TessSharedStats`.

Where the core may use a Parallel Hash (`enable_parallel_hash`), the hook
offers a second partial path with a shared table: the inner side's
partial path divides the build among the participants too, into one
table in the query's dynamic shared memory; its template is the core's
Parallel Hash join, and the table may take every participant's
`hash_mem`, as the core's does. The cheaper of the two wins. A right or
a full join takes the second only, as the core does: with a table each,
every participant would return the records without a pair.

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
clause in turn whether it runs in batches, whether the inner side is unique,
the planner's estimate of its rows and whether the table is shared
(version 6).

### Execution

At its first execution the node derives both children's requests from
its parent's: the outer columns asked for come from the outer batches,
the inner ones are kept in the table, and each side gives its keys
first. It then reads every inner batch, hashes the keys in order, each
further key folded into the first one's hash, with the NULL policy of a
join (a NULL in any key never matches) and appends the rows as records
to chunks with `tess_table_append`, the first chunk of 64 kB and the
others of 1 MB, adding one when the last is full. A record's payload is
a word of the NULL bits of the kept inner columns and a Datum per
column. A by-reference value is copied into the table's value chunks,
the first of 64 kB and the others of 1 MB (a value larger than a quarter
of one into a chunk of its own, an expanded object flattened, a TOAST
pointer as it is), and the payload word holds its reference, the chunk's
number plus one and the byte in it, 0 for NULL: no address of a process,
so the words mean the same in every process and, for spilling (plan item
5.6), on disk. A round's values are turned into addresses through the
chunks' bases when a parent first asks for the column. The node also notes which kept columns hold
a NULL at all. Once the inner side is read, the node makes the index for
exactly the rows appended and links every chunk with
`tess_table_link_grouped`, which puts a key's records next to each
other in their chain and counts the records whose key was there
already. No estimate sizes anything and nothing is copied: an inner side
larger than the planner thought takes more chunks. An empty inner side
ends the scan without reading the outer child.

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
inner columns from those records. An outer column is read over the
first round's rows only, since the later rounds are among them, so a
lazy child reads no value of a row without a pair. Published over the outer batch, a key
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

With a shared table the node keeps, in its chunk of the query's DSM, the
build's `Barrier`, the build's counters and the table's index, chunk
directory and lists in the query's dynamic shared memory, and steps its
participant through the phases of `tess_build_step` (see
[table.md](table.md)): every participant appends the inner batches its
partial scan hands it to chunks of its own in dynamic shared memory,
each numbered by `tess_build_take_chunk` and entered in a list of its
own, which only it adds to until the elected one reads them all, and
reports its records; the elected one makes the
index for exactly the records appended and the directory of the chunks'
`dsa_pointer`s by number, from which every participant maps their
bases; each participant links its own chunks with `tess_table_link`.
Nothing is copied and the table never grows. By-reference inner values
go into value chunks as in a table of its own, allocated in the query's
dynamic shared memory: each participant fills chunks of its own,
numbered under a spinlock and entered in a list of its own, by which the
table frees them; the elected one makes their directory with the
records', and every participant maps their bases, so a reference means
the same in each.
The barrier's waits stay in the node, since they may raise an error. Then
every participant probes the one table; its chains are not grouped, so
the next record of a key is found by `tess_table_next_match`. The links
count the table's duplicates (`tess_table_link` with its count), unless
the planner knows the inner side unique: a table without them has no
second round, and one with them goes out in compact batches as a serial
table does. The shared Bloom filter is sized with the table: each
participant decides on it by its own batches as before, the first that
wants it builds it for all (`tess_table_try_build_bloom`), and every
participant checks batches against it once it reads it ready, probing
without it until then. Each participant leaves at shutdown or at a rescan, and
the last one to leave frees the table; a rescan of the `Gather` builds
anew. A worker that attaches once the build is over only probes, and one
that attaches after the last one left returns nothing. Without a DSM (a
`Gather` that launched no workers) the node builds a table of its own
from the whole inner side, which its partial scan then reads alone.

### Right and full joins

A right join runs as an inner join and a full join as a left one, the
inner side kept too: its rows with a NULL key become records as well,
hashed as a key of their own, which no probe finds. Every published pair
that passed the join clauses marks its record, in a bit per record per
chunk the node keeps (a reference is a chunk's number and a place in
8-byte units, `TESS_TABLE_UNIT_BITS`). After the outer side the node
walks each chunk's records up to its used mark and returns those without
a mark, their outer columns NULL, through the outer join's filters. A
table that spills does the same for each table it holds in memory before
that one goes: the resident partitions' after the outer side, a
partition's after its outer rows, each piece of a partition joined in
passes after its pass, and a level's resident partitions after the outer
rows of the partition it split; a partition with inner rows and no outer
ones is loaded for its tail alone. Every table built or loaded starts
without marks. A shared table's marks are in the query's shared memory,
a bit per record of the largest chunk for each chunk, allocated with the
directory by the participant SIZE elects (a round's by the one that
allocates it), and set with an atomic OR after a plain read finds the
bit clear. Its tail goes out as the core's Parallel Hash Right Join
returns its unmatched rows: a participant done with its outer rows
leaves the table (the build barrier, or the round's) without waiting,
and only the last one to leave returns the records without a mark, then
frees the table, the free it owes done at its next leave; the others'
marks are complete once they detached. The partitions on disk of a
shared table that one participant joins alone are its own tables, as a
serial join's. A rescan that keeps the table clears the marks.

### Spilling

A table of the node's own spills once it would outgrow `hash_mem` (see
[spill.md](spill.md)): its chunks past the first take at most an eighth
of `hash_mem`, and before a batch that could take the table past it
with a chunk of records and one of values, the node chooses a power of
two of partitions, from 4 to 1024, that makes the inner side it expects
(twice what it read, or the planner's rows if more) about half of
`hash_mem` each, as long as a tail per partition and side still fits in
half of it. The records read so far are split into the partitions by
their hashes' low bits (`tess_table_split`), their by-reference values
copied into value chunks of their partitions, and the rest of the inner
side is appended partitioned from the batch's columns
(`tess_table_append_partitioned_columns`), which counts each partition's
rows; the node then only copies the by-reference values. Every
partition starts resident, in memory whole; while the partitions,
their values, a Bloom filter of every inner row and room for the outer
side's tails take more than `hash_mem`, the largest resident partition
goes to disk: its value chunks, then its chunks of records, each to the
partition's temporary file, its last chunk staying as its tail. A
partition on disk writes its tail when it fills, after the value chunks
opened since the last one, so that a file read in order gives values
before the records that refer to them.

Once the inner side is read, the resident partitions make the table the
outer batches probe. An outer row whose partition is on disk leaves the
probe: without an inner row in its partition, or rejected by the filter
of every inner row, it has no pair and is answered at once (a left or
anti join returns it); otherwise it is appended as a record of its
partition to a table of the outer keys, whose payload is the NULL bits
and the outer columns the node reads, a by-reference value copied into
the partition's value chunks right after its row, and the rows the
batch answers now leave out those written. When the outer child is
done, each partition with outer rows written is joined in turn: its
chunks and values are read back, with its tail, and indexed as a table
of their own, and its outer rows come back in batches of up to 64 rows
gathered from their records, which take the same way as the outer
child's batches, rounds, compact batches, clauses, and the rows of left
and anti joins without a pair. A compact batch holding pairs of one
table keeps the next from being loaded until it goes out, and a table
with duplicates the resident one lacked turns compact mode on.

A partition whose file is larger than what `hash_mem` leaves, and that
holds less than nine tenths of the inner rows its level split, so that
it is no single key, splits again while hash bits are left: its inner
rows are read back group by group (value chunks, then the chunks of
records that refer to them), and then its tail, each chunk split by the
bits above its level's into a level of partitions of its own, which
start resident and go to disk as the first level's do; that level then
probes with the partition's outer rows as its outer side, joins its own
partitions in turn, splitting further where needed, and hands back to
the level above. Memory counts the tails every level keeps, and the
spilled chunks are allocated in blocks of their own size.

Any other partition whose file is larger than what `hash_mem` leaves is
joined in pieces: its blocks are read back in order until the piece passes the
room, ending only where a group ends (value chunks, then the chunks of
records that refer to them; without by-reference columns, any chunk),
the tail with the last piece, and every piece is joined with all of the
partition's outer rows, read again from their first. A left, semi or
anti join keeps a bit per outer row of the partition, in the order read,
set once the row finds a pair: a semi join's row with one takes no
further part, and left and anti joins answer the rows without one in a
last pass without a table. Both sides append their rows the same way, a
by-reference value going into its partition's value chunks right after
its row, so that a chunk's values are written with it or before it. A
rescan of a table that spilled reads the inner side again.

A shared table spills past every participant's `hash_mem` (see
[spill.md](spill.md), "A shared table"): the first participant past the
budget splits it, every participant splits its own chunks when it sees
that and appends partitioned, and the largest partition goes to disk
while the chunks take more, each participant writing its own chunks of
it. The `FLUSH` phase writes the tails of the partitions on disk and
hands the chunks of the others to the table, which `SIZE` indexes for
them alone; the `OUTER` phase writes every outer row before any goes
out, those of the partitions on disk to their files and the others to
a file the shared table answers, left and anti joins' rows without a
pair among them. At `PROBE` each participant probes the shared table
with those rows and leaves it. A partition on disk that fits in one
participant's `hash_mem` is then a round every participant takes part
in (`tess_round_step`, a barrier per partition): the elected one makes
its index in shared memory, all load its blocks from the files they
take and link them, all probe with the outer files they take, and the
last to leave frees it; a larger one is joined by the participant that
takes it whole, from every participant's files, with the code above. The
partitions' records and values carry the numbers every participant
shares, so a participant reads another's files as its own.

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

A probe that finds no record still reads a bucket, and on a table past
the cache that is a cache miss. The node therefore counts the valid
probe rows of each table built and those that found a record: after the
first 4096, if fewer than a share of them found one (the setting
`tessera.join_bloom_ratio`, 0.5 by default) and the table holds at least
4096 rows, it builds a Bloom filter of the table's keys once
(`tess_table_bloom`, see [table.md](table.md)), 16 bits per row in the
table's memory context, and from then on checks every batch against it
first (`tess_bloom_probe`), probing the table only with the rows it lets
through. A row it rejects has no pair, which a left or anti join returns
as it does a probe miss. The decision is the batches' fact, not the
planner's estimate of the join's selectivity, and holds until the table
is built again; a smaller table stays in the cache, where a miss costs
less than the check. Under a `Gather` each participant decides on the
filter of its own table by its own rows. The setting at 1 builds the
filter at the first batch whatever the sizes, at 0 never. An inner or
semi join, which drops a row without a pair and whose keys are all words
(the child has a hashed key's value, not its hash), then hands the filter to
its outer child (`tess_input_set_key_filter`): a TessFilter with row-wise
clauses takes it and removes the rows it rejects before those clauses
run, and the join checks no more; it takes the filter back before the
table goes. A left or anti join returns those rows and keeps the filter.

`EXPLAIN` shows the join type for a semi, anti, left, right or full join, the key
clauses as `Hash Cond`, `Shared Table` for a shared table, the residual ones that run in batches as `Batch
Join Filter` and the others as `Join Filter`, and an outer join's filters
as `Batch Filter` and `Filter`. With `ANALYZE` it adds
the bucket count of the last table built, `Memory Usage`, the most the
table, the copies of inner values and spilling took, `Overrun`, what of
it exceeded `hash_mem` (shown only then: a partition larger than it is
joined whole), `Builds`, the tables built over the rescans, `Build Rows`,
the inner rows inserted into them, `Chunks`, the chunks of their
records, and for a table that spilled `Batches`, its partitions,
`Resident Partitions`, those kept in memory, `Spilled Chunks` and `Disk
Usage`, the blocks and bytes written by both sides, and `Tail Chunks
Kept`, the tails joined without being written, `Split Partitions`, the
partitions split into a level below, `Extra Passes`, the
passes over outer rows past the first of a partition joined in pieces,
and for a shared table `Partitions Joined Together`, the rounds, and
`Partitions Joined Alone`, the partitions one participant took whole;
`Probe Rows`, the outer
rows probed, and `Matches`, the joined
rows over every round, `Rows Removed by Join Filter` and `Rows Removed by
Filter`, and `Compact Batches`, the batches of copied
pairs, when there are any, and `Bloom Filters`, the filters built, with
`Rows Removed by Bloom Filter`, the valid probe rows they rejected, when
one was built (not shown when the outer child took the filter and the
join removed none), and `Bloom Filter Below` when it did; with a shared table `Builds` counts the one build, and
`Memory Usage` each participant's chunks and value blocks, and the index
and filter of the elected one. Under a `Gather` the counters are the totals of
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
planner's estimate, which takes more chunks; a top-N sort and a limit
above the node, and a scrollable cursor through `Material`. With
`EXPLAIN ANALYZE`, the memory shown as within `hash_mem` or over it (its bytes depend on the allocator and differ in an assert build), it shows the counters of a join, of
one with rounds and of an inner side of 20000 rows estimated at 10, which goes
past a small `work_mem`; correlated subqueries whose parameter is on the
inner side, which builds the table for every outer row, and on the outer
side, which builds it once; and a generic plan executed with two
parameters. Under a `Gather` with two workers, the core's shared hash
table disabled, it compares an aggregate over the node's partial path,
rows through the `Gather`, rounds, and the leader not taking part, and
checks that the rows probed and the matches are the totals of every
participant. It also shows
the core's plan without the kernels module, for a full join, a text key
the node takes with its equality as a join filter, a key over an
expression, hash joins disabled and the switch off; `types.sql` compares
joins by text and numeric keys of every kind, with an integer key, NULL
keys, numeric 1.0 against 1.00, a case-insensitive collation, spilling,
the shared table and no Bloom filter below.
Spilling, at a `work_mem` of 512 kB: an inner side of about 3 MB with
duplicates, NULL keys and text, joined with the counters shown for an
inner and a left join, and compared as rows with text of both sides, a
left join with misses and NULL keys, semi and anti joins, a residual
clause, one key held by 60000 inner rows the planner expects 10 of,
joined in pieces as inner, left, semi and anti joins and with a residual
clause, 200000 inner rows with text it expects 10 of, whose partitions
split into a level below, under the same joins, a rescan with a parameter of the outer side, and under the `Gather` each participant spilling its own
table. A shared table spilling at a `work_mem` of 256 kB (every
participant's `hash_mem` together about half of the inner side): inner
and left joins with text of both sides and a residual clause, one key of
60000 rows the planner does not expect, semi and anti joins over the
larger side, the workers alone, a rescan of the `Gather`, and no
temporary file left; at 1 MB, partitions joined in rounds, with a key of
100000 rows statistics do not show, under every join kind, a residual
clause, the workers alone and a rescan.
Semi, anti and left joins: `EXISTS` with and without a join clause, `IN`
over a subquery, `NOT EXISTS` with and without one (NULL keys going
out), a left join the planner turns into an anti join, left joins with
misses and NULL keys, with duplicates as rows and in compact mode under
an aggregate, with a join clause in `ON` and a filter in `WHERE`, under a
sort; an empty inner side for each kind; rescans with a parameter in the
join clauses; and under the `Gather` a left, a semi and an anti join.
The Bloom filter: with `EXPLAIN ANALYZE`, a build side of 5000 keys that
about 1 % of the probe rows find builds one for an inner, an anti and a
left join, one whose probe rows all find a key builds none, nor does a
build side of 300 rows, and an inner side with a parameter builds one
per table; the rows of inner, semi, anti and left joins through the
filter are compared, and under the `Gather` an inner and an anti join,
where the participants build at least one filter. With `enable_parallel_hash` on, the
shared table: inner, semi, anti and left joins compared with the core,
duplicate keys, an inner side past the estimate, the leader not taking
part, a rescan of the `Gather` from a correlated subquery, the counters
of that build (one build, every row, an index sized for them all), one
shared Bloom filter built for all, and by-reference inner
columns: text in the target and in a join clause, numeric and text with
NULLs over many value blocks under inner and left joins, a table with
text past the estimate, and a rescan that frees the blocks and fills new ones.

## TessSort

`TessSort` (`nodes/sort.c`) stands in for the core's full `Sort` under
`ORDER BY`. It reads every batch of its batch child into `TessRows`
(records of the table format, [runtime.md](runtime.md), "Keeping rows"),
each with the sort keys in its slots and every output column in its
payload, sorts the records with the kernels ([table.md](table.md),
"Sorting records") and returns them in order.

### Planning

The module's `create_upper_paths` hook looks at the ordered relation: each
of the core's `SortPath`s there, also one under a `ProjectionPath`, is
replaced in place by the node's path over `tess_batch_input_path` of the
sort's input, with the same rows, costs and path keys, when

- every path key orders by an expression of the sort's input target of a
  type a key's word holds through the operator family whose order the
  word keeps (`tess_word_key_order`): int2, int4 and int8 through the
  integer one, date, timestamp and timestamptz through `datetime_ops`,
  bool through its own, or of any type with an ordering operator of the
  path key's operator family (see Other types below), ascending or
  descending, NULLs first or last, at most 16 keys; the first key of
  another type has an abbreviated key the node takes, unless a word key
  comes before it;
- the output has 1 to 1664 columns, a tuple's most;
- the query is not `FETCH ... WITH TIES`, which passes no bound, and the
  kernels module is loaded.

Rows past `work_mem` are sorted into runs on disk and merged (see
External sort below), so the planner's estimate of the rows is no gate.

A sort under a `Gather Merge`, a `SortPath` over a partial path as the
core plans it for a parallel ordered scan, becomes the node's path too:
every participant sorts its share, in memory or in runs of its own, and
`TessGatherMerge` (below) merges them in place of the `Gather Merge`. That path is parallel-aware only for the
counters the participants share (`TessSharedStats`: EXPLAIN sums rows,
runs, disk and memory, and the overrun past each one's `work_mem`). The
path keeps the core's costs; the ordered stage also gets the node's sort
of the cheapest partial path under `TessGatherMerge` at the batch
gather's cost of a row (below), which the core's `Gather Merge` of the
same sort may have lost to a serial sort already. `IncrementalSort` stays
with the core.
The plan's layout is dense, one column per target; the private data gives
each target's column in the child's batches and each key's target, kind
(-1 for another type), flags, ordering operator and collation, and `custom_exprs` holds the keys' expressions for `EXPLAIN`.
The path supports backward scan; mark/restore it does not, so a merge
join puts `Materialize` above it.

### Execution

The first execution reads the child forward to its end, whatever the
direction of the fetch, and appends every batch's selected rows; a key
takes the bit for NULL only when one of its rows held a NULL, so a single
int4 key without NULLs sorts as one word per row. A batch-aware parent
then gets batches of 64 rows in order, whose columns are gathered from
the records when it asks for them; a row-wise parent gets rows one by one,
forward or backward, from windows of 64 rows. A rescan without a changed
parameter returns the sorted rows again from the first; a changed
parameter of the child reads and sorts it anew. `EXPLAIN` shows the keys
as the core does; `ANALYZE` adds the method, the memory (records, values,
index, items and references at the sort) and its overrun past `work_mem`,
and the batches and rows read.

### Other types

A key of a type whose word the kernels do not order (numeric, text, uuid,
float8, ...) orders by its type's sort support
(`PrepareSortSupportFromOrderingOp` with the path key's ordering
operator, collation and place of NULLs). The kernels order the keys up to
the first such key, and its word in the records' slots is its abbreviated
key made a signed int8 in its order: the comparisons of unsigned 64- or
32-bit integers (text, uuid, macaddr, inet, bytea) flip the top bit or
extend, signed ones stay, and numeric's, a signed integer's reversed, is
inverted (a type check, since its comparison is its own static function,
checked on two values); a type without an abbreviated key has the word 0.
The converter runs per batch in a context reset per batch; the node never
aborts abbreviation, as the core may when it saves little. After the
kernels sorted the items, each run of items whose words are equal but
for the reference is a group, whose rows are ordered in C by the type's
comparison of that key and every key after it: the first key's value in
a 16-byte row as the core's `SortTuple` keeps it, the others' gathered
from the records, sorted by the core's sort template
(`lib/sort_template.h`, comparisons inline). The runs' starts are marked
in a bitmap first, so that the items are freed before the group's rows
take their memory. An external sort orders each run the same way, and
merges runs in C: a binary heap of the inputs, made anew for every 64
rows, ordered by their next rows' lanes and then by the comparisons, a
row's values read from its block. A first key of such a type without an
abbreviated key makes every row one group, which the node sorts as the
core's sort does, 2 to 7 % slower (text under a libc collation on macOS,
2 M rows): the planner leaves that case to the core
(`generic_abbreviates`, sort support prepared in a context of its own).
`EXPLAIN` shows a key's collation when it is not the default, as the core
does. Under a limit the top-N heap of such keys is the node's, in C (see
Top-N). At 2 M rows in memory: numeric 125 ms against 296, text under
`"C"` 187 against 366, text of the default collation after an int4 of
1000 values 532 against 742 (bench/pg sort, pg-sort-WXjAaO).

### External sort

When the rows in memory, what sorting them takes and a chunk more pass
`work_mem`, the rows so far are sorted and written as a run, and memory
is freed for the next. `TessRows` then takes chunks of an eighth of
`work_mem`, not of 1 MB, so that runs fill it evenly. Every key of an
external sort's items has its bit for NULL, so that every run's items
have one width. A run is a set of its own ([spill.md](spill.md), "Files")
of block pairs: a block of the by-reference values of some rows, one
after another, and a chunk of columns (`TESS_SPILL_COLUMNS`) of those
rows: a lane of the output columns' NULL bits per 64 stored words, a lane per output column,
a by-value Datum or a value's byte in its block of values, and a lane per
word of the rows' items without the reference, which the merge compares:
the last word goes when it holds no key's bits (an int4 key's 33 bits
merge as one word, not two), else the reference's bits are 0. With
by-value columns only, a column goes into its lane a run of rows at a
time. A block holds a 128th of `work_mem`, 64 rows at least. The runs of
the input are the partitions of one set of files, and each pass's runs
of another: a file per run made closing and deleting them 8 % of the
sort, and 4116 runs at 64 kB twice the core's time.

After the input, the runs merge with the kernel `tess_sort_merge`
(`tessera/sort.h`): each run's current block in memory, its key lanes
from its next row on, the kernel putting out the run of each row in order
through a loser tree it keeps between calls in the node's words, and
stopping when a run's block is done with more of it to come, which the
node then reads. (A heap built anew for every 64 rows took 30 % of the
sort; a head per run cached as a u128 and compared without branches was
10 % slower than reading the lanes.) A merge takes as many runs as a block pair each fits
`work_mem` (the pairs a batch put out points into stay only until the
next batch), 6 at least as the core's does (a small `work_mem` is passed
then) and 256 at most; more runs merge in passes into longer runs first.
At 4 MB a merge takes 64 runs: 2 M rows make 41 to 55 and merge once.
The last merge streams: every batch of 64 rows takes its columns from the
runs' blocks, a by-reference value pointing into its block of values, and
the blocks a batch points into are freed with the next batch. A plan that
may scan backward (`EXEC_FLAG_BACKWARD`) merges into one run instead,
whose blocks are read by their positions, a window within one block, in
either direction. A rescan without a changed parameter starts the last
merge again over the runs on disk; a changed one reads the child anew.
`EXPLAIN ANALYZE` shows `Sort Method: external merge`, the disk written,
the runs and the passes. At a `work_mem` of 4 MB, 2 M rows sort in 0.35
to 0.55 of the core's time (an int4 key and column 81 ms against 203,
two keys 116 against 328, with text 128 against 232; in memory the first
takes 58); at 64 kB, where the runs are small and the passes many, 249
against 390, and with text 535 against 510.

### Top-N

A limit above (`TessLimit`) passes the node its bound, the count plus the
offset, through `set_tuple_bound` before it fetches. When the bound's heap
and the records appended before a rebuild fit `work_mem`, the node keeps
the best rows in a max-heap of their items (`tessera/sort.h`), every key
with its bit for NULL so that the item's width never changes: a batch's
key columns are read first, and once the heap is full only the rows whose
keys beat the worst kept stay in the batch's mask, so only they have
their other columns read, are appended and go into the heap. When the
records outnumber four times the bound or 65536, whichever is more, the
rows are made anew from the heap's (`Rows Rebuilt` in `EXPLAIN ANALYZE`),
so that keys in the reverse of the order, each row beating the ones kept,
take bounded memory. At the end the heap's items sorted give the rows in
order. A rescan with a bound larger than the rows kept reads the child
again; a smaller one returns the first of them. `Sort Method: top-N in
memory`.

With a key of another type the heap is the node's, in C, as the core's
bounded heap: slots of the items' words (the kernels' layout, from
`tess_sort_key_lanes` over a batch's keys) and the values of the keys
from the first generic one on, pointers into the records, with a spare
slot for the row coming in. Once the heap is full a batch keeps the rows
whose lanes order before the worst's, or, when they are equal, whose
values do by the comparisons; those are appended and pushed one by one.
A rebuild keeps the slots in place with their new references and values.
At the end the kernels sort the heap's items and the groups of equal
words are ordered as in a full sort. The first 10 of 2 M rows: numeric
90 ms against 159, text under `"C"` 49 against 94 (pg-sort-kLXeKY).

### Tests

`test/sql/sort.sql` compares the rows of every query in order with
Tessera on and off: int4 and int8 keys ascending and descending with
NULLs first and last, three keys, a key the query does not return, a key
the child computes; over a filter keeping no row, one, 64, 65 and more;
text carried along; a join below, packed; a limit with an offset above,
which reads the node's batches; 100000 rows with text of up to 300 bytes;
a merge join above through `Materialize`. At a `work_mem` of 64 kB,
runs and passes: 100000 rows with text, every class of key both ways
with NULLs first and last, several keys, equal keys, an empty and a
one-row input, rescans with and without a changed parameter, and a
scrollable cursor over one run. It shows the core's plan
without the kernels module, for a first float8 key and under `LIMIT`; a scrollable cursor forward and backward across windows and
past both ends, and one whose first fetch is backward; a correlated
subquery whose parameter reaches the child, sorted for every outer row,
and one whose parameter stays above the node, read once; and a generic
plan with a parameter. `test/sql/types.sql` compares sorts by numeric (NaN
and infinities, 1.0 and 1.00 equal), text under `"C"` both ways with long
common prefixes, uuid, two keys of other types, text and float8 after an
integer key, an expression; the core's sort for a first float8 key and
under `LIMIT`; at 64 kB external merges of such keys and a scrollable
cursor over one run; a rescan with a new parameter; workers under the
core's `Gather Merge`; top-N with equal words (1.0 and 1.00, a long
common prefix), an offset, a bound past the rows, a bound of 0, rebuilds
with the best rows arriving first. Mutations each fail it: groups left
unsorted, the kernels' merge by words, numeric's words not inverted, the
comparison of the first generic key only, a merge by words only, and for
top-N the batch filter or the heap by words only, the heap's groups
left unsorted, a rebuild that keeps the old references. A rebuild that
keeps the old values reads freed memory that still holds them in a
build without assertions, and passes.

## TessAppend

`TessAppend` (`nodes/append.c`) stands in for the core's `Append` under a
batch parent: it reads its batch children in turn and gives the parent
each child's batches as they are. Under the core's `Append` every child's
batches became rows, and a pack node above made batches of them again:
13 ms of an aggregate's 31 over two filtered scans of a million rows each
(the scans alone took 18).

### Planning

The node has no hook. It publishes `wrap_append`, which
`tess_batch_input_path` calls for an `Append` path under a batch parent,
so the node never stands under a row-wise one, where the core's `Append`
passes rows at no cost of its own, and it survives the core rebuilding a
partitioned table's paths after the scan/join target is applied. It takes
the `Append` of a base relation's children (a partitioned table, an
inheritance tree, a `UNION ALL` the planner made a relation of) or of a
set operation's branches, in a plain `SELECT` without row marks, when the
path is not parameterized, has two children at least, each of which has
a batch path (`tess_batch_input_path` again) and one of which at least
does more than pack rows, and when the core would not prune partitions
while executing: a clause over a partition key with a parameter or a
function that is not immutable keeps the core's `Append`, which prunes
by it (the node would read every partition, correctly but slower). The
path copies the `Append`'s properties, parallel ones included, and costs
it less the core's half of `cpu_tuple_cost` a row.

The plan's layout is dense, a column per target; each child's plan has
as many targets, in the same order. The relation's clauses, which the
core passes to a custom scan of it, are the children's to evaluate: the
core translated them to every child. A set operation's columns become the
first child's targets, as `EXPLAIN` of the core's `Append` shows them.

### Execution

Each child is read through a batch input, which gets the parent's request
in the child's own columns (every column for a row-wise parent). The
batch given out is the node's own: the child's row mask and table, and
`get_datum_column` that renumbers the column by the child's layout and
asks the child's batch, so nothing is copied. A bound from a limit above
goes to every child, as `ExecSetTupleBound` passes it to the children of
an `Append`. A rescan passes the changed parameters to the children,
rescans them and starts from the first.

In a parallel plan the path is parallel-aware as the core's Parallel
Append is, and the node shares its children out the same way: in its
chunk of the query's shared memory, after the rows of its counter
(`TessSharedStats`, the batches), a lock, the child a worker looks at
first and a flag per child that needs no more participants. A worker
takes the first unfinished child from there on, going round to the first
partial child; the leader takes them from the last down; a child that is
not partial is finished as soon as someone takes it, a partial one when
someone reaches its end, while the others still reading it go on. Without
shared memory the node reads every child in turn. `EXPLAIN ANALYZE` shows
the `Batches` given out, every participant's.

### Tests

`test/sql/union.sql` shows the plans and compares every result with
Tessera off: `UNION ALL` of two and of five branches (an empty table, a
branch without rows, a constant target, a plain scan), columns in another
order in each branch, nested `UNION ALL`, branches of int4 and int8,
branches of core scans only (the core's `Append`, packed), rows to a
row-wise parent (the core's `Append`), a hash join and a sort above,
a limit whose bound reaches a top-N sort in each branch, a correlated
subquery and an initplan; an inheritance tree; partitions, one of them
partitioned again, pruned while planning to two and to one, by a
parameter while executing (the core's `Append`) and a parameter over
another column (the node); the parallel plans: the children shared out,
a child without a partial path, three of them with a partial one, the
workers alone, a rescan under `TessGather`, and a partial `Append` that
is not parallel-aware. `UNION` without `ALL` over the node, with NULL,
duplicates, two columns and three branches, `EXPLAIN VERBOSE`, a sort and
a limit above, in a subquery, spilling at a `work_mem` of 64 kB; a `UNION`
within another set operation stays the core's. `INTERSECT` and `EXCEPT`
with and without `ALL`: NULL keys, duplicates on both sides, an empty
side, keys of int2, date, text and numeric, sides of other types, a group
of 2000 copies, numeric 1.0 against 1.000, a sort and a limit above, one
within another set operation (the core's inside), both kinds of spill at
64 kB, a correlated subquery. Mutations fail it: the right side's
constant 0, a group's copies cut at a batch, `EXCEPT ALL` the left rows
alone, the right side only probing a table that spilled. A mutation that skips resetting the shared memory on a
rescan gives a wrong result there; one that leaves a child that is not
partial unfinished once a worker takes it does not show: another
participant reads it again only while the worker is still reading it.

## TessGather, TessGatherMerge and TessSend

`TessGather` (`nodes/gather.c`) stands in for the core's `Gather` over a
batch subtree, `TessGatherMerge` for its `Gather Merge`, and `TessSend` is
the subtree's top in every worker. The
core's `Gather` passes rows one by one: a worker forms a minimal tuple of
each and puts it into a queue, the leader reads and deforms it. Through
it 1.33 M rows of a filtered scan took 27 to 30 ms with two workers,
against 12.4 ms in one process: the parallel plan was slower than the
serial one.

### Planning

The module's `create_upper_paths` hook, at the final relation, walks the
tree of each of its paths (projections, sorts, aggregates, grouping
sets, window functions, unique, limit, row locks, material, memoize,
joins, appends, set operations, recursive unions, modifications, the
subqueries of min/max aggregates, which the core plans without the final
stage, and custom paths) and replaces a `GatherPath` whose child is a
Tessera path by `TessGather` over `TessSend` over that child, with the
same rows and costs, when

- the gather plans workers (a single copy runs in one worker without the
  leader; the core makes one only as the plan-level `Gather` of
  `debug_parallel_query`, outside the paths, and it stays);
- the child is parallel-safe, unparameterized, of a column at least;
- `tessera.batch_gather` is on (the default).

A projection the core put under the gather, over a Tessera node that
projects (`dummypp`), goes to a copy of that node, which computes the
target in the workers, as the core's plan would. A gather that projects
itself, of a target the workers may not compute (a parallel-restricted
function), becomes a projection over `TessGather`, computed in the leader
as the core's `Gather` computes it.

Once a query has executor parameters (an initplan, a subplan, a
recursive CTE, a set operation), `finalize_plan` requires one of the
core's Gathers or Gather Merges over every parallel-aware plan node, for
the rescan parameter it adds, and knows no custom scan that gathers. So
`TessGather`'s plan holds back the flag of the parallel-aware nodes under
it, and the module's `planner_hook` gives it back once `standard_planner`
is done: the flag gets a node its shared-memory callbacks, and the rescan
parameter the node does without, rescanning its child and reinitializing
the shared memory itself.

A `GatherMergePath` over a Tessera path becomes `TessGatherMerge` over
`TessSend` the same way when every path key is one `TessSort` takes: a
target of a type a key's word holds through the family whose order the
word keeps, or of another type by its ordering operator, the first such
key with an abbreviated key unless a word key comes before it (as for
`TessSort`, "Other types", at most 16 keys), and the kernels module is
loaded; TessSend's data then lists each key's target, kind, flags,
ordering operator and collation.

That replacement keeps the core's costs, and a `Gather` the core costs at
`parallel_tuple_cost` a row has often lost to a serial path by then.
So the module also offers the node's own paths where the core gathers,
at a quarter of `parallel_tuple_cost` a row (13.3 M rows took the leader
at most 3.5 ns each through `TessGather` against 14 through the core's
`Gather` over the same nodes): the `set_rel_pathlist` and
`set_join_pathlist` hooks add `TessGather` over a base or join
relation's cheapest partial path when that is a Tessera path, before the
core gathers the same partial path at its own cost, and the ordered
stage's hook adds `TessGatherMerge` over the node's sort of the cheapest
partial path; `add_path` keeps the cheaper. At the default costs, 2 M
rows of `bench_sort` ordered by an int4 key then run in parallel, 61 ms
against 82 for the serial `TessSort` chosen before, `ORDER BY ... LIMIT
10` 10 ms against 32 for the core's `Gather Merge` over `TessSort`s that
get no bound, and a scan returning 200000 of the rows 7 ms against 8.

A `TessPack` above the gather goes, since `TessGather` gives batches. The
plan sets `parallelModeNeeded`; `TessGather`'s layout is dense, one column
per target, and `TessSend` keeps its child's.

### Execution

`TessSend` is parallel-aware: in the leader it puts into its chunk of the
query's shared memory a queue of 256 kB per worker, the leader its
receiver. In a worker it reads its child's batches and copies their
selected rows into a message: a header (rows, columns, the lanes' stride,
the bytes of values), a lane of the rows' NULL bits per 64 columns (column c
takes bit c % 64 of lane c / 64), a lane of words per
column, a by-value Datum or a value's byte offset in the message, and the
by-reference values' bytes. The rows of a message are as many as a
quarter of the queue holds in lanes, 64 to 1024; a message is sent when
its rows are full or its values pass half the queue. Rows go to no
parent; when the leader has detached from the queue (a limit above was
met), the worker stops.

`TessGather` launches the workers as `ExecGather` does
(`ExecInitParallelPlan` with the child's external parameters,
`LaunchParallelWorkers`, the queues attached with the workers' handles).
It reads the queues in turn without waiting and gives its parent each
message as batches of up to 64 rows whose columns point into the message.
While every queue is empty and the leader takes part
(`parallel_leader_participation`), it reads `TessSend`'s child itself as
a batch input; when it does not, it waits on its latch. It ends when every
queue is detached and its own share is read. Without launched workers the
leader reads the whole child. The shutdown detaches the queues first, so
that a worker blocked on a full one stops (a limit above met), then
finishes the workers and
moves their counters into the plan's nodes for `EXPLAIN ANALYZE`; a
rescan shuts them down and rescans the child, and the next fetch launches
anew. A bound set by a limit above (`set_tuple_bound`) goes to the
leader's child and, through TessSend's shared memory, to every worker's,
so that a sort below keeps a top-N heap in each participant; the core's
`Gather Merge` passes no bound to a custom scan. `EXPLAIN` shows
`Workers Planned`, `ANALYZE` `Workers Launched`, and `VERBOSE` the
messages and the rows from the workers and of the leader.

### Merging

Under `TessGatherMerge` a message carries, after the columns' lanes, a
lane per word of the rows' sort items as the runs of an external
`TessSort` keep them (every key with its bit for NULL, the reference left
out, and the last word when it holds only the reference's bits): the
kernel `tess_sort_key_lanes` (`tessera/sort.h`) writes them for a batch's
selected rows from its key columns. The leader copies its own rows into
messages of the same form, first, so that it sorts its share while the
workers sort theirs; then it waits for a message of every worker and
merges the streams with `tess_sort_merge` through a loser tree kept
between calls. A batch takes up to 64 rows in order, its columns copied
into the node's arrays, a by-reference value pointing into its message; a
stream whose message ran out loads the next only when the next batch is
asked for, once the rows pointing into it are consumed, so the merge
stops at a stream's last row in hand and the batch goes out shorter.

With a key of another type the lanes hold the keys up to it, its word
its abbreviated key (each process makes them alike: the node never
aborts abbreviation), and the leader merges in C: a binary heap of the
streams with rows in hand, made anew for every batch, ordered by their
next rows' lanes and then by the comparisons of that key and the ones
after it, their values read from the messages; the batch stops at a
stream's last row in hand as above. A limit's bound reaches the workers'
`TessSort`s, which keep top-N heaps. With two workers, 2 M rows: numeric
118 ms against 291 for the core (serial 125), text under `"C"` 152
against 385, the first 10 rows by numeric 38 against 66, where the core's
`Limit` over its `Gather Merge` over the node's sorts took 57
(pg-sort-w2-lg8fvZ).

With two workers, 2 M rows ordered by an int4 key take 61 ms against 100
for the core's `Gather Merge` over the same `TessSort`s and 81 for a
serial `TessSort` (with text 76 against 120 and 113; two keys 77 against
130 and 118); with four, 54 against 105. The leader then merges at about
16 ns a row while the workers wait on full queues, and sorts its own
share before: with `parallel_leader_participation` off, four workers take
44 ms. Under `LIMIT 10` 17 ms against 29, the workers keeping a top-N
heap.

On 20 M rows of which 13.3 M pass the gather, one process takes 124 ms;
with two workers `TessGather` takes 67 ms against 250 to 274 for the
core's `Gather` over the same nodes and 317 for the core's plan, with four
47 against 188 and 218.

### Tests

`test/sql/sort.sql` compares the rows of `TessGatherMerge` in order with
Tessera off (`test/sql/types.sql` for keys of other types: numeric,
text under `"C"`, two such keys, text after an integer, limits, the
workers alone over 200000 rows whose words are equal, past `work_mem`;
a merge by the words only fails it): keys of both kinds, both directions and places of NULL, text
and values of 2000 bytes carried along, sorts past `work_mem`, the
workers alone, a limit and an offset, a rescan, and the core's
`Gather Merge` with `tessera.batch_gather` off.
`test/sql/parallel.sql` shows the plans and compares the rows with the
serial plan's, and at the default costs the plans that go parallel only
through the node's own paths: scans, filters, joins and aggregates under `TessGather`;
200000 rows with NULLs, text and values of 2000 bytes that fill a
message's half of its queue before its rows do; twenty columns, which
narrow a message's rows; the workers alone; a limit that stops them while
they send; an error in a worker; and the core's `Gather` with
`tessera.batch_gather` off.
