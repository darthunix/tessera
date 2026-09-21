# Runtime library

`libtessera_runtime.a` (`runtime/`) holds the helpers a batch node needs
beyond the bridge's contract: building batches from rows, the node's output
and input sides, the unary node helper over both, the named plan-data codec,
the path and plan helpers, and the batch expression compiler. It is a static library, installed next to the bridge in
`pkglibdir` with its header `tessera/runtime.h`; a node module links it
rather than calling through the bridge, so the bridge stays a small contract
and the helpers can change with the nodes that use them:

```make
MODULE_big = my_node
SHLIB_LINK += $(shell $(PG_CONFIG) --pkglibdir)/libtessera_runtime.a
```

## Building a batch from rows

`TessBuilder` collects rows from tuple slots into an owned column-major
Datum batch. A node whose child returns rows, such as the pack node,
creates one builder for the scan:

```c
TessBuilderConfig config = TESS_STRUCT_INITIALIZER(TessBuilderConfig);

config.parent_context = estate->es_query_cxt;
config.tuple_desc = ExecGetResultType(child);
config.ncolumns = ncolumns;
config.capacity = 64;
builder = tess_builder_create(&config);
```

and fills a batch per `ExecProcNode` cycle:

```c
tess_builder_reset(builder);
while (!tess_builder_is_full(builder))
{
    TupleTableSlot *slot = ExecProcNode(child);

    if (TupIsNull(slot))
        break;
    tess_builder_append_slot(builder, slot);
}
batch = tess_builder_finish(builder, InvalidOid);   /* NULL without rows */
```

`append_slot` materializes the leading `ncolumns` attributes of the slot
with `slot_getsomeattrs`, which works for every kind of slot: a virtual slot
is already deformed, a heap or minimal-tuple slot deforms just that prefix.
A builder with zero columns only counts rows, for a parent that needs no
values, as a scan under `count(*)` does.
Pass-by-reference values are copied into the builder's own memory context,
because a slot's values point into the tuple's memory, which the child's
next call may free; so the slot may be cleared or reused right after the
append. A batch of pass-by-value columns is copied without that context.

The finished batch selects all its rows, holds every row initialized (a NULL
row stores 0), exposes only `get_datum_column`, whose result covers the
whole column for any requested mask, and has no release callback: the
builder owns the storage and `reset` reuses it. Kernels therefore read the
builder's columns with `prepared = NULL`. Before `reset`, the node takes the
previous batch off its slot binding; the builder does not check that, the
bridge does when the next batch is published. Finishing again returns the
same batch; `is_full` is true after `capacity` rows or after finishing.

Errors are raised with `ERROR`: a configuration without a context or
descriptor, a column count outside `0..natts`, a capacity below one, an
append after finishing or into a full builder, a slot with fewer
attributes than columns, and a column request out of range or with an
undersized result structure.

## Keeping heap tuples

`TessHeapBatch` is the provider for a child that returns buffer heap tuple
slots, as a sequential scan without projection does. Instead of copying
columns it keeps each row as a tuple header pointing into its page, pins
the page once per batch with `IncrBufferRefCount` until the batch is
released, and deforms a column only when a consumer asks for it through
`get_datum_column`, for the rows the mask names that were not deformed
before; a row from any other slot is copied as a tuple into a per-batch
context. The batch therefore exposes `release`, which drops the pins, and
by-reference values point into the tuples until then:

```c
TessHeapBatchConfig config = TESS_STRUCT_INITIALIZER(TessHeapBatchConfig);

config.parent_context = estate->es_query_cxt;
config.ncolumns = ncolumns;
config.capacity = 64;
heap = tess_heap_batch_create(&config);
...
tess_heap_batch_reset(heap);
while (!tess_heap_batch_is_full(heap))
    tess_heap_batch_append_slot(heap, ExecProcNode(child));   /* not NULL */
batch = tess_heap_batch_finish(heap, InvalidOid);
```

A scan that reads pages itself appends the visible tuples of a page in one
call with `tess_heap_batch_append_page`, by their line pointers, which
pins the page once and fills the rows in a plain loop; single tuples go
by their headers through `tess_heap_batch_append_tuple`, pinning their
page or copying a tuple given without one. Such a scan supplies the
descriptor and the guaranteed prefix in the configuration instead of a
first slot.

Deformation resumes: every row keeps a `TessDeformCursor`
(`tessera/heap_deform.h`), the attributes passed so far and the byte
offset in the tuple data, and `tess_deform_advance` moves it to one target
attribute without materializing the ones in between, through cached
offsets while they hold and by walking past the first variable-length or
NULL attribute; a column before a row's cursor is deformed from the row's
start with a local cursor and counted as restarted. A by-value column at
a cached offset is read at that offset in a plain loop with the width
fixed per call, for the rows whose tuple has the attribute and no NULL
before it, leaving their cursors alone, since a later column starts from
the cached offsets anyway; such reads are never counted as restarted, and
the other rows go through the cursor. The cursor is the
deformation of PostgreSQL's own slots reduced to one target and made
resumable, ported from pg_batch; it relies only on public inline helpers,
so PostgreSQL is not patched. A tuple shorter than the descriptor yields
the missing value, as a slot does. `tess_heap_batch_stats` gives the
deformed and restarted values and the copied rows for `EXPLAIN`. The
guaranteed prefix (`tts_first_nonguaranteed` of the first slot) and the
descriptor are taken from the slots appended, so a node needs no
descriptor of its own.

## Computing columns on demand

`TessProjection` gives a batch computed columns: a wrapper around a child's
batch whose columns come first, with one more column per computed target,
evaluated when a consumer asks for it and for the rows it asks for. It is
how a node with `CUSTOMPATH_SUPPORT_PROJECTION` publishes the expressions
PostgreSQL installs in its target list (see `TESS_LAYOUT_PROJECTED` under
"Building plans"):

```c
TessProjectionConfig config = TESS_STRUCT_INITIALIZER(TessProjectionConfig);

config.parent_context = estate->es_query_cxt;
config.parent = &css->ss.ps;
config.econtext = css->ss.ps.ps_ExprContext;
config.scan_slot = css->ss.ss_ScanTupleSlot;
config.scan_tuple = &scan_tuple_layout;
config.base_columns = child_layout.ncolumns;
config.computed = info.computed;
projection = tess_projection_create(&config);
...
batch = tess_projection_wrap(projection, child_batch);
```

A target the [expression compiler](expr.md) accepts, a column and
registered calls over it with constants and parameters, is computed by
its batch chain over the batch's selected rows the first time a consumer
asks for it, once per batch, and the chain's own result column is handed
out without a copy. The wrapper shares the child's row mask, so a filter or
a limit above that narrows the wrapper before asking narrows what the
chain computes. A released wrapper forgets the child, which stays the
node's to finish through its input; the next `tess_projection_wrap` takes
the next batch, and `tess_projection_reset` forgets one at a rescan. `tess_projection_stats` counts the values
computed for `EXPLAIN`.

Any other target, a text expression, a cast, a `CASE`, a function, is
computed row by row by the executor: `ExecInitExpr` at creation, and per
request the scan tuple slot is filled, for each row asked for and not
computed yet, with only the attributes the expression reads, fetched from
the child for those rows, and `ExecEvalExprSwitchContext` evaluates it in
an expression context of the projection's own, whose per-tuple memory is
the wrapped batch's: a by-reference result is handed out without a copy
and lives until the wrapper is released, when that memory is reset, and a
read-write expanded object is made read-only, as the executor's projection
makes its results. The rows computed are remembered per column, so a
filter above and the projection never compute a row twice, and an
expression that fails, a division by zero at some row, fails only when a
consumer asks for that row, as the core's `Result` would evaluating output
rows only.

## Publishing batches and serving rows

`TessOutput` is the output side of a node: a virtual slot bound to the
bridge. The node creates it in `BeginCustomScan` on its result slot, with
the layout that maps every slot attribute to a batch column (extra batch
columns for resjunk targets are allowed), and passes its `PlanState` so
that the helper can adjust the instrumentation the executor allocates
after `Begin`:

```c
output = tess_output_create(estate->es_query_cxt, &node->ss.ps,
                            node->ss.ps.ps_ResultTupleSlot, &layout);
```

The parent configures the request through `tess_output_binding`; the node
reads it, frozen, with `tess_output_request`. Each `ExecProcNode` of a node
that owns batch storage then goes:

```c
tess_output_release(output);            /* the parent finished the last batch */
tess_builder_reset(builder);
... fill ...
batch = tess_builder_finish(builder, InvalidOid);
if (batch == NULL)
    return NULL;                        /* end of input */
return tess_output_publish(output, batch);
```

`publish` leaves the slot non-empty, so a parent never sees the end of the
input by mistake: in row mode it shows the batch's first selected row, in
batch mode an all-NULL row, since a batch-aware parent reads the batch
through the binding and never looks at the slot's values, which would cost
a lazy provider the whole first row of every batch. A batch with no
selected rows cannot be published, the node skips it. A batch-aware parent
finds the binding through the slot and reads the batch; it marks the batch
consumed when done, and the node's next `release` or `publish` returns the
storage, refusing while the batch is unconsumed. A row-wise parent sees one row per call: the node walks the
selection with `tess_row_mask_next`, shows each row with
`tess_output_select`, and marks the batch consumed itself with
`tess_output_finish` before fetching the next.

In batch mode one `ExecProcNode` call returns the whole batch, which the
executor's instrumentation counts as one row; `publish` therefore adds the
batch's other selected rows to `instrument->tuplecount`, so `EXPLAIN
ANALYZE` reports the rows a batch node produced. In row mode nothing is
adjusted. `tess_output_clear` releases an active batch, finished or not, for
the end and rescan paths; `tess_output_end` also detaches the binding and
must precede destroying the slot.

`tess_runtime_api()` returns the bridge's API, validated once per backend;
the bridge must be loaded first (`CREATE EXTENSION tessera`).

## Reading batches from a child

`TessInput` is the input side of a node over one batch-producing child.
The node creates it in `BeginCustomScan` after `ExecInitNode` of the child,
whose result slot then carries the binding, and sends its request before
the first fetch:

```c
child = ExecInitNode(outerPlan(cscan), estate, eflags);
input = tess_input_create(estate->es_query_cxt, child);
request.filter_columns = ...;
request.output_mode = TESS_OUTPUT_BATCH;
tess_input_set_request(input, &request);
```

Each cycle fetches a batch, works on it and finishes it; `NULL` ends the
input:

```c
while ((batch = tess_input_next(input)) != NULL)
{
    ... clear rows, read columns ...
    tess_input_finish(input);
}
```

`next` refuses while the previous batch is unfinished, unless a forwarding
parent finished it directly through the bridge. A child that forwards its
own child's batch returns that child's slot; the input finds the binding of
whatever slot it receives and caches it by slot pointer. The batch belongs
to the child, which releases it when it publishes the next one or ends.

Rescan follows the node contract: the node clears its own output, finishes
its input, calls `ExecReScan` on the child, and then `tess_input_rescan`,
which forgets the cached slot and binding.

## Unary nodes

`TessUnary` joins the output and input sides for the common node with one
batch child that only removes rows from the child's batches: a limit, a
filter. A node that builds a new physical batch, changes the column layout
or has several children uses the output and input helpers directly.

The node initializes its child, keeps it in `custom_ps` and creates the
helper in `BeginCustomScan`:

```c
child = ExecInitNode(linitial(cscan->custom_plans), estate, eflags);
css->custom_ps = list_make1(child);
config.parent_context = estate->es_query_cxt;
config.node = css;
config.child = child;
config.layout = &plan.layout;
config.filter_columns = columns_read_while_processing;
config.process = trim_batch;
config.private_data = state;
unary = tess_unary_create(&config);
```

`create` binds the node's result slot, which is where the parent sends its
request, and wraps the child, which must be a batch node. Nothing is ever
published through the node's own binding: a batch can be on one binding
only, and the child's batches stay on the child's.

The first execution derives the child's request from the parent's, frozen
at that point, and the node's own: the filter and projection columns are
the unions of both, the batch limit is the smaller of the two nonzero
limits, and the child is always asked for batches. The node's layout must
have the child's number of columns, since batches pass through unchanged.
`tess_unary_child_request` returns what was sent.

Each fetch takes the child's next batch, hands it to `process`, which
clears rows from the mask and returns how many remain, counts the removed
rows as filtered in the node's instrumentation, and skips a batch left
without rows. For a batch-aware parent, execution returns the child's slot
with the batch: the parent finds the binding of that slot and finishes the
batch there, and the helper adds the batch's other rows to the node's
instrumentation, since one call returned them all. For an ordinary parent,
the helper serves the batch's rows one per call from the node's own slot:
it adds the batch column of every slot attribute to the child's projection
request, reads those columns once per batch and copies one row at a time,
so a node needs no row-wise code of its own. `tess_unary_stop` ends the
input early, as a limit does once it is satisfied; execution then returns
`NULL`.

A node that needs only the first rows, such as a limit, tells the child
so with `tess_unary_set_tuple_bound`, the helper's form of
`ExecSetTupleBound`: a batch child whose node kind takes bounds (the
optional `set_tuple_bound` callback of its `TessNode`) receives it and
forwards it below, as the pack node does, so a sort under the limit stays a
top-N sort and the pack node pulls no more rows than the bound; any other
child goes to `ExecSetTupleBound` itself.

`tess_unary_rescan` performs the node contract's whole rescan order: it
clears the node's output, finishes the input, rescans the child, resets the
input and the helper's counters. `tess_unary_end` detaches the node's
binding; the node ends the child itself.

## Counters over parallel participants

A node's counters (batches, pages, kernel calls) live in its backend, and
`EXPLAIN ANALYZE` reads the leader's node: in a parallel query the
workers' counts would be lost, while PostgreSQL sums its own
instrumentation over the workers. `TessSharedStats` lays out one row of
counters per participant in the node's chunk of the query's shared memory,
the leader's row first, so that the leader can report the totals.

PostgreSQL calls the shared memory callbacks of a custom scan only for a
plan marked parallel-aware, so a node with such counters declares its
partial path parallel-aware, whether or not it shares anything else, and
installs the five callbacks:

```c
/* EstimateDSMCustomScan */
return tess_shared_stats_estimate(NCOUNTERS, pcxt->nworkers);
/* InitializeDSMCustomScan */
state->stats = tess_shared_stats_init(estate->es_query_cxt, coordinate,
                                      NCOUNTERS, pcxt->nworkers, pcxt->seg);
/* ReInitializeDSMCustomScan */
tess_shared_stats_reset(state->stats);
/* InitializeWorkerCustomScan */
state->stats = tess_shared_stats_attach(estate->es_query_cxt, coordinate,
                                        ParallelWorkerNumber + 1);
/* ShutdownCustomScan */
tess_shared_stats_store(state->stats, values);
```

A node with other shared state, such as a parallel scan descriptor, puts
the rows after it in the same chunk and adds the estimate to its size.

Every participant stores its counters into its row when its node shuts
down, which the executor does after the last row of a plan, in the leader
and in every worker. The leader sums the rows into a backend-local array
when the segment is detached: the `Gather` above waits for every worker to
finish before it destroys the segment, and when a limit above the `Gather`
stops it early, the leader detaches its ends of the tuple queues first,
which ends the workers' plans normally, so the totals are complete either
way. When the leader's node ends while the rows are still mapped, as the
children of a `Gather` end before it destroys the segment, `end` sums them
then and cancels the callback. `EXPLAIN` prints the totals when
`tess_shared_stats_totals` returns them and the node's own counters
otherwise, in a serial plan. A rescan of the `Gather` reinitializes the
chunk and zeroes every row.

## Named plan data

A `CustomPath` and a `CustomScan` carry a node's private data in
`custom_private`, a `List` that PostgreSQL copies with `copyObject` for
cached plans and serializes with `nodeToString` for parallel workers. The
codec in `tessera/plan.h` stores that data as named, typed fields behind a
record kind and version, so that a node reads back exactly what it wrote
and never silently ignores a field. A path's or plan's private data is
written once, when the path or plan is created:

```c
List *
make_limit_data(Node *offset, Node *count)
{
    TessPlanWriter *writer = tess_plan_writer_create("my.limit", 1);

    tess_plan_write_node(writer, "offset", offset);
    tess_plan_write_node(writer, "count", count);
    tess_plan_write_int(writer, "scan_direction", 1);
    return tess_plan_writer_finish(writer);
}
```

and read back with the same kind and version, in any order:

```c
TessPlanReader *reader = tess_plan_reader_create(scan->custom_private,
                                                 "my.limit", 1);

direction = tess_plan_read_int(reader, "scan_direction");
offset = tess_plan_read_node(reader, "offset");
count = tess_plan_read_node(reader, "count");
tess_plan_reader_finish(reader);
```

Fields hold an `int`, a string, a node (copied, `NULL` allowed), a `List`
of nodes, an `IntList`, or a `Bitmapset` stored as the list of its members.
Values are copied when written; a reader returns borrowed pointers into the
record, except a bitmap, which is rebuilt.

The writer refuses an empty name, a duplicate name, a `NULL` string, a list
of the wrong kind and any write after `finish`. The reader refuses data
without the header, another kind, another version, a malformed or duplicate
field, a missing field, a field of another type and a field read twice;
`finish` raises an error naming the first field that was never read.
Increment the record version when required fields or their meaning change,
so that a plan cached by an older module revision is rejected instead of
misread. `tess_plan_data_kind` returns a record's kind, or `NULL` without an
error for a list that is not a record, which lets a node tell its own paths
from other custom paths.

## Building paths

A node module owns its planner hooks, path selection and costs. The
helpers in `tessera/planner.h` only build the `CustomPath` the same way for
every batch node, so that other nodes recognize a batch path. Start from
the core path whose planner properties the node keeps, with the cost
already adjusted by the node:

```c
TessPathConfig config = TESS_STRUCT_INITIALIZER(TessPathConfig);
Path template = *seqscan_path;

template.total_cost *= 0.9;
config.template_path = &template;
config.methods = &my_path_methods;
config.node = &my_node;
config.children = list_make1(seqscan_path);
config.expressions = list_make1(limit_count);
config.flags = CUSTOMPATH_SUPPORT_PROJECTION;
add_path(rel, (Path *) tess_path_create(&config));
```

`tess_path_create` copies rows, costs, path keys and parallel properties
from the template, and refuses a parameterized one, since a batch path is
never the inner side of a nested loop. A partial path is built the same
way from the core's partial path, whose number of workers and rows per
participant come along; a node that installs no shared memory callbacks
clears `parallel_aware` afterwards, and the helpers below over a partial
core path give a partial batch path. `node` is the registered kind of
node that owns the path; the helper stores its name, `expressions` and
`node_data` through the plan-data codec. `tess_path_node` returns that node
for a path built here and `NULL` for any other path, including a custom
path of another provider, which is how a node tells a batch child from a
row-producing one. `tess_path_get_info` reads the stored expressions and
data back in `PlanCustomPath`, and `tess_path_matches` recognizes a node's
own paths by their methods. Never `copyObject` a path: PostgreSQL does not
copy path nodes, and neither does this library.

A batch parent may stand above any core path: `tess_batch_input_path`
returns a batch path over the path it is given, the path itself when it is
one, the heap scan node's path (through the `scan_rows` callback registered
under `TESS_HEAP_SCAN_NODE_NAME`) when the path is a sequential scan of a
plain heap table without clauses, and otherwise the pack node's path over
it, through the `wrap_rows` callback registered under `TESS_PACK_NODE_NAME`.
A parent that evaluates the relation's clauses itself, as the filter node
does, asks `tess_batch_scan_path` for the native scan first. Both return
`NULL` for a parameterized path, for a relation with a pseudoconstant clause
(the planner gates every scan of it with a `Result` the parent could not
read through), or when no node kind takes it, and the parent then adds no
path:

```c
Path *child = tess_batch_input_path(root, copy_of_seqscan);

if (child == NULL)
    return;
config.children = list_make1(child);
```

A parent that adds its path to the same relation as the child copies the
child first (`*copy = *seqscan`): `add_path` frees a core path that the new
path dominates, and the wrapped child must outlive it.

The pack node sees through a subquery scan: a `SubqueryScanPath` without
clauses, whose targets are columns of the subquery and whose subquery is
planned as a batch path, gets a pack that forwards the batches of the plan
under the scan instead of packing its rows (see `TessPack` in
[nodes.md](nodes.md)), so that a subquery with `LIMIT`, which the planner
cannot pull up, does not break a batch chain into rows.

## Building plans

`PlanCustomPath` turns the path into a `CustomScan` the same way for every
batch node:

```c
static Plan *
plan_custom_path(PlannerInfo *root, RelOptInfo *rel, CustomPath *best_path,
                 List *tlist, List *clauses, List *custom_plans)
{
    TessPathInfo path = TESS_STRUCT_INITIALIZER(TessPathInfo);
    TessPlanConfig config = TESS_STRUCT_INITIALIZER(TessPlanConfig);
    Plan *child = linitial(custom_plans);

    tess_path_get_info(best_path, &path);
    config.methods = &my_scan_methods;
    config.layout_policy = TESS_LAYOUT_DENSE;
    config.scan_targetlist = child->targetlist;
    config.expressions = path.expressions;
    return tess_plan_create(best_path, tlist, custom_plans, &config);
}
```

`tess_plan_create` sets the methods, the path's flags, `scanrelid`, and
copies of the target list, `qual`, `expressions` (as `custom_exprs`) and
`scan_targetlist` (as `custom_scan_tlist`), and records the owning node,
which children are batch nodes, the output layout and `node_data` through
the plan-data codec. PostgreSQL copies costs and relids from the path
afterwards.

The output layout has four policies. `TESS_LAYOUT_DENSE` publishes one
column per entry of the final target list; it is derived again whenever the
plan is read, because a node with `CUSTOMPATH_SUPPORT_PROJECTION` gets its
target list replaced after `PlanCustomPath` returns, which then receives an
empty list. `TESS_LAYOUT_EXPLICIT` copies the layout the node supplies.
`TESS_LAYOUT_PRESERVE_CHILD` keeps the layout of the batch child at
`layout_child`, for a pass-through node whose target list is the child's;
a row-producing child there is an error. `TESS_LAYOUT_PROJECTED` is for a
node with `CUSTOMPATH_SUPPORT_PROJECTION` whose batches have more columns
than its targets: the node supplies the layout of its scan tuple, one
target per `scan_targetlist` entry, or per attribute of the relation with
`scan_tuple_is_relation` (then the plan has no `custom_scan_tlist`); the
final targets are derived whenever the plan is read, a target that is a
scan tuple entry (equal to it before setrefs, an `INDEX_VAR` after; a
column of the relation in the relation mode) maps to that entry's column,
and every other target is a computed column after the scan tuple's, listed
in `TessPlanInfo.computed` for the node to evaluate (see "Computing
columns on demand" below). `tess_plan_child` describes one
child the same way for a node's own planning: its path, plan, the
registered node or `NULL` for ordinary rows, and the batch child's layout.

A node without a scan relation (`scanrelid` 0) describes its scan tuple in
`scan_targetlist`, usually its child's target list; the executor evaluates
the final target list against that tuple. `NULL` takes the preserved
child's target list, or otherwise the plan's own, for a node whose scan
tuple is its output. Qualifiers and expressions
are never inferred: set them to exactly what the executor evaluates. A
wrapper around a complete child path passes no clauses, since the child
already enforces them.

`BeginCustomScan` reads the plan once:

```c
TessPlanInfo plan = TESS_STRUCT_INITIALIZER(TessPlanInfo);

tess_plan_get_info(cscan, &plan);
input = tess_input_create(estate->es_query_cxt, child);
output = tess_output_create(estate->es_query_cxt, &css->ss.ps, slot,
                            &plan.layout);
```

`child_names` holds the registered name of each batch child and `NULL` for
a child that produces ordinary rows. `tess_plan_get_layout` returns the
layout alone, for a parent reading a child's plan. Both allocate the names
and the layout's map for the caller.

Register the scan methods with `RegisterCustomScanMethods` in `_PG_init`: a
parallel worker reads the plan back from its text form and finds the methods
by name. The [planner test](../test/tessera_planner_test.c) is a complete
forwarding node built this way, with a `set_rel_pathlist` hook that calls
the previous hook, checks `tessera.enable` and wraps a sequential scan.

## Batch expressions

`tessera/expr.h` compiles a PostgreSQL expression over one batch column
into a chain of calls of the functions the registry implements, and
evaluates it over a batch's selected rows as a value column or as a filter
that narrows the row mask in place. The [expression guide](expr.md)
describes the language, what the planner checks and what a node does per
batch.
