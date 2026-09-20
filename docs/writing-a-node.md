# Writing a batch node

This guide walks through a batch node from the planner hook to the
regression test, using `TessLimit` (`examples/limit/`) as the running
example: a node built against the public headers and the runtime library
alone, as a node of another extension would be. The rules every node keeps
are in [node.md](node.md); the helpers are described in
[runtime.md](runtime.md); the bridge's contract in [bridge.md](bridge.md).

## The execution model

A batch node is a `CustomScan`. One `ExecProcNode` call returns a whole
batch: the node publishes it on the binding of its result slot and returns
that slot, and a batch-aware parent finds the binding through the slot,
reads the batch's columns as Datum arrays or through a native interface,
clears rows from its mask and marks it consumed. A parent that is not
batch-aware gets ordinary rows, one per call, served from the same slot.
Before execution, the parent sends a request to the child through the
same binding: which columns it needs while filtering and after, whether it
wants batches or rows, and how many rows a batch may hold. A node that only
removes rows from its child's batches forwards the child's slot upward
instead of publishing a batch of its own.

Every batch node binds `ps_ResultTupleSlot` in `BeginCustomScan`; that is
where a parent finds the request binding. Plan data travels in
`custom_private` through the named codec, so cached plans and parallel
workers read exactly what the planner wrote.

## Registering the node

The module registers its kind of node with the bridge and its scan methods
with PostgreSQL, and installs its planner hook, calling the previous hook
first and checking `tessera.enable` before adding paths:

```c
const TessNode tess_limit_node = {
    TESS_ABI_INITIALIZER(TESS_NODE_ABI_VERSION, TessNode),
    .name = TESS_LIMIT_NODE_NAME,
};

void
_PG_init(void)
{
    const TessApi *api = tess_runtime_api();

    api->nodes->add(&tess_limit_node);
    RegisterCustomScanMethods(&tess_limit_scan_methods);
    previous_create_upper_paths_hook = create_upper_paths_hook;
    create_upper_paths_hook = create_upper_paths;
}
```

`tess_runtime_api` raises an error when the bridge is not loaded, so a
missing `CREATE EXTENSION tessera` or a wrong preload order fails at load
time. A node module defines no setting of its own: `tessera.enable`, which
the bridge defines, turns every batch node off.

## Building the path

A hook decides where the node applies and builds its path from the core
path it replaces or stands above. `tess_path_create` copies rows, costs,
path keys and parallel properties from the template, refuses a
parameterized one, and records the node's name and data. A batch parent
gets a batch child over any path from `tess_batch_input_path`: the path
itself when it is a batch path, the heap scan node's path for a sequential
scan of a plain heap table without clauses, otherwise the pack node's path
over it, or `NULL` when nothing can be done, in which case the hook adds no
path:

```c
static CustomPath *
make_limit_path(PlannerInfo *root, LimitPath *limit)
{
    TessPathConfig config = TESS_STRUCT_INITIALIZER(TessPathConfig);
    Path *child = tess_batch_input_path(root, limit->subpath);

    if (child == NULL)
        return NULL;
    config.template_path = &limit->path;
    config.methods = &limit_path_methods;
    config.node = &tess_limit_node;
    config.children = list_make1(child);
    config.expressions = list_make2(limit->limitOffset, limit->limitCount);
    return tess_path_create(&config);
}
```

`TessLimit` replaces the core limit path in the final relation's path list
in place (`lfirst(lc) = path`), which frees nothing. A hook that adds its
path to the same relation as the child through `add_path` copies the child
first, since `add_path` frees a core path that the new path dominates.

## Building the plan

`PlanCustomPath` reads the path's data back and describes the scan:

```c
static Plan *
limit_plan(PlannerInfo *root, RelOptInfo *rel, CustomPath *best_path,
           List *tlist, List *clauses, List *custom_plans)
{
    TessPathInfo info = TESS_STRUCT_INITIALIZER(TessPathInfo);
    TessPlanConfig config = TESS_STRUCT_INITIALIZER(TessPlanConfig);

    tess_path_get_info(best_path, &info);
    config.methods = &tess_limit_scan_methods;
    config.layout_policy = TESS_LAYOUT_PRESERVE_CHILD;
    config.layout_child = 0;
    config.expressions = info.expressions;
    return tess_plan_create(best_path, tlist, custom_plans, &config);
}
```

The layout policy says which columns the node's batches expose:
`TESS_LAYOUT_PRESERVE_CHILD` for a pass-through node whose target list is
its child's, `TESS_LAYOUT_DENSE` for one column per target, derived again
whenever the plan is read since PostgreSQL may replace the target list
after `PlanCustomPath`, and `TESS_LAYOUT_EXPLICIT` for a layout the node
supplies. Qualifiers and expressions are never inferred: the node sets
exactly what the executor evaluates, and a wrapper around a complete child
path passes no clauses, since the child already enforces them.

## The short path: a unary node

A node with one batch child that only removes rows stands on the unary
helper, which takes the parent's request, derives the child's, fetches
batches, hands each to the node's callback, skips batches left empty, and
serves a batch-aware parent the child's slot or an ordinary parent the
rows. `BeginCustomScan` initializes the child, keeps it in `custom_ps`, and
creates the helper:

```c
tess_plan_get_info(cscan, &info);
child = ExecInitNode(linitial(cscan->custom_plans), estate, eflags);
css->custom_ps = list_make1(child);
config.parent_context = estate->es_query_cxt;
config.node = css;
config.child = child;
config.layout = &info.layout;
config.process = trim_batch;
config.private_data = state;
state->unary = tess_unary_create(&config);
```

The callback clears rows from the batch's mask and returns how many
remain; a limit clears the offset's rows and the rows past the count, and
stops the input once the count is reached:

```c
static int
trim_batch(void *private_data, TessBatch *batch, int rows)
{
    ...
    while ((row = tess_row_mask_next(&batch->rows, row)) >= 0)
        if (state->offset_remaining > 0)
        {
            tess_row_mask_clear(&batch->rows, row);
            state->offset_remaining--;
        }
        ...
    if (!state->no_count && state->count_remaining == 0)
        tess_unary_stop(state->unary);
    return kept;
}
```

Execution, rescan and end are one call each: `tess_unary_exec`,
`tess_unary_rescan`, which performs the node contract's rescan order, and
`tess_unary_end` followed by `ExecEndNode` of the child. A node that needs
only the first rows calls `tess_unary_set_tuple_bound` once its count is
known: a batch child whose kind takes bounds forwards it below, as the
pack node does, so a sort under a limit stays a top-N sort and the pack
node pulls no more rows than the bound. `TessLimit` evaluates its
expressions at the first execution after every rescan, since a parameter
may have changed, and raises the core node's errors for negative values.

`ExplainCustomScan` reports the node's own counters; under `ANALYZE` the
helper's statistics give the batches and rows read and the rows kept. The
node never touches instrumentation itself.

`TessFilter` (`nodes/filter.c`, `nodes/planner.c`) is the second node on
the helper and the first with a `set_rel_pathlist` hook: it takes the
clauses of a base relation, applies the leading ones the expression
compiler supports as batch filters and the rest row by row through
`ExecQual`, and keeps the relation's target through an explicit layout
while the scan below reads the clauses' columns too. See
[nodes.md](nodes.md) for how it takes the clauses away from the scan.

## Nodes that create batches

A node that builds a new physical batch, changes the column layout or has
several children uses the output and input helpers directly: the builder
turns rows into an owned column-major batch, the heap batch keeps a scan's
buffer tuples and deforms columns on request, the output publishes batches
and serves rows to an ordinary parent, and one input per batch child sends
the request and fetches. The pack node (`nodes/pack.c`) is the model: it
binds its result slot in `BeginCustomScan`, creates the provider at the
first execution once the parent's request is frozen and the child's first
slot is known, and publishes each batch through the output helper, which
corrects the instrumentation.

## Testing and measuring

Each node ships the regression tests listed in [node.md](node.md): a
batch-aware parent and a row-wise one, an empty selection, batches of fewer
and of more than 64 rows, rescan, an early stop by a limit above, NULL
values, `EXPLAIN ANALYZE` with correct row counts, and lifecycle errors.
`test/sql/limit.sql` is the model; it needs no test module, and
`test/sql/filter.sql` shows how to compare a node's rows with the core's
through one function that runs a query with `tessera.enable` on and off. Performance is
measured with the PostgreSQL-level benchmarks in `bench/pg/`, with
`tessera.enable` on and off in one session.
