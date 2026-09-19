# Runtime library

`libtessera_runtime.a` (`runtime/`) holds the helpers a batch node needs
beyond the bridge's contract: building batches from rows, the node's output
and input sides, the named plan-data codec, and later the unary node
helper. It is a static library, installed next to the bridge in
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
descriptor, a column count outside `1..natts`, a capacity below one, an
append after finishing or into a full builder, a slot with fewer
attributes than columns, and a column request out of range or with an
undersized result structure.

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

`publish` shows the batch's first selected row in the slot, so the slot is
never empty for a parent; a batch with no selected rows cannot be published,
the node skips it. A batch-aware parent finds the binding through the slot
and reads the batch; it marks the batch consumed when done, and the node's
next `release` or `publish` returns the storage, refusing while the batch
is unconsumed. A row-wise parent sees one row per call: the node walks the
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
never the inner side of a nested loop. `node` is the registered kind of
node that owns the path; the helper stores its name, `expressions` and
`node_data` through the plan-data codec. `tess_path_node` returns that node
for a path built here and `NULL` for any other path, including a custom
path of another provider, which is how a node tells a batch child from a
row-producing one. `tess_path_get_info` reads the stored expressions and
data back in `PlanCustomPath`, and `tess_path_matches` recognizes a node's
own paths by their methods. Never `copyObject` a path: PostgreSQL does not
copy path nodes, and neither does this library.

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

The output layout has two policies. `TESS_LAYOUT_DENSE` publishes one
column per entry of the final target list; it is derived again whenever the
plan is read, because a node with `CUSTOMPATH_SUPPORT_PROJECTION` gets its
target list replaced after `PlanCustomPath` returns, which then receives an
empty list. `TESS_LAYOUT_EXPLICIT` copies the layout the node supplies.

A node without a scan relation (`scanrelid` 0) describes its scan tuple in
`scan_targetlist`, usually its child's target list; the executor evaluates
the final target list against that tuple. `NULL` keeps the plan's own target
list, for a node whose scan tuple is its output. Qualifiers and expressions
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
