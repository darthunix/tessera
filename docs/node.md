# Node registry

The node registry lets independently built extensions register and find
kinds of batch-producing nodes by stable name. A `TessNode` describes a kind
of node, not the state of one query execution: its identity, and optional
planning and execution callbacks as trailing fields. `wrap_rows` is
published by the pack node so that a batch parent in any module can stand
above any core path, and `scan_rows` by the heap scan node so that such a
parent reads a plain heap table in batches without the pack, and
`wrap_append` by the append node so that such a parent reads the children
of an `Append` in batches (see [runtime.md](runtime.md), "Building
paths"); `set_tuple_bound` lets a node
kind take the bound a limit above passes down, as `ExecSetTupleBound`
tells the core nodes, and forward it to its child.

## Finding the registry

After the `tessera` bridge has been loaded, get the registry during module
initialization and keep its operation table:

```c
const TessApi *api = *find_rendezvous_variable(TESS_API_RENDEZVOUS);
const TessNodeRegistryOps *nodes;

if (api == NULL || api->abi_version != TESS_API_ABI_VERSION ||
    api->struct_size < TESS_API_MIN_SIZE)
    elog(ERROR, "incompatible Tessera API");

nodes = api->nodes;
if (nodes == NULL ||
    nodes->abi_version != TESS_NODE_REGISTRY_OPS_ABI_VERSION ||
    nodes->struct_size < TESS_NODE_REGISTRY_OPS_MIN_SIZE)
    elog(ERROR, "incompatible Tessera node registry");
```

`nodes` is a required, non-null field covered by `TESS_API_MIN_SIZE`.
Reading it after validating the root does not require a separate
`TESS_ABI_HAS_FIELD` check. The registry table has its own version and size.

## Registration and lookup

The provider defines a description with static storage and registers it:

```c
static const TessNode my_node = {
    TESS_ABI_INITIALIZER(TESS_NODE_ABI_VERSION, TessNode),
    .name = "my_extension.limit",
};

nodes->add(&my_node);
```

`nodes->find("my_extension.limit")` returns the registered pointer or `NULL`.
An absent, null, or empty lookup name returns `NULL`.

Names are nonempty, case-sensitive, and unique among nodes in one PostgreSQL
backend. Registering the same pointer again is safe. Another description with
the same name is an error. A null description, incompatible ABI version or
size, or a null or empty registration name is also an error. Descriptions
with additional trailing fields are accepted; a consumer checks an optional
callback such as `wrap_rows` with `TESS_ABI_HAS_FIELD` before calling it.

Source and node names belong to separate registries. A source and a node may
use the same name; changing one registry does not affect the other. Each
PostgreSQL backend has its own registrations, including parallel workers.

## Ownership and removal

The provider owns the `TessNode` structure and the string referenced by
`name`. The registry stores the pointer without copying the structure or name
string. The structure and name string must remain valid and unchanged from
registration until `remove` returns.

The pointer returned by `find` is borrowed. The consumer must not modify or
free the description or its name, and finding it does not extend its
lifetime. Before removing a description, the provider must ensure that all
consumers have finished using their borrowed pointers:

```c
nodes->remove(&my_node);
```

`remove` unregisters only this exact object. A different object with the same
name cannot remove it. An unregistered object, `NULL`, and repeated calls for
an object that is still alive are safe. Once an object has been removed, its
name can be registered again.

Removal does not wait for consumers or free the description or its name.
After removal, consumers must stop using previously returned pointers. The
provider may then free dynamic allocations with the matching allocator. The
static description and string literal above need no explicit freeing. Do
not pass an old pointer to `remove` after freeing its object.

## Obligations of every batch node

A kind of node registered here is a `CustomScan` provider; Tessera's own
nodes live in the nodes module ([nodes.md](nodes.md)). Beyond the
registry, every batch node in Tessera keeps the rules below, so that the
shared runtime helpers can rely on them and every node is tested the same
way. The rules follow pg_batch's nodes (`nodes/*.c`, `runtime/*.c` and
`docs/writing-a-node.md` there) except where a Tessera decision is stated.

### Shape

A node is a `CustomScan` with `CustomPathMethods`, `CustomScanMethods` and
`CustomExecMethods`. `ExecProcNode` returns a `TupleTableSlot` attached to
the bridge: when the parent requested batch output, the slot carries the
published batch; otherwise the node returns one row per call. A batch node
attaches its binding to `ps_ResultTupleSlot` in `BeginCustomScan`; a parent
finds the request binding there. A node that forwards its child's batch
returns that child's slot, and a parent looks up the binding of whatever
slot it receives. A node that declares `CUSTOMPATH_SUPPORT_PROJECTION`
learns its targets in `BeginCustomScan`, since PostgreSQL installs a
projection after `PlanCustomPath` returns: it plans with the projected
layout and computes the targets that are not columns of its scan tuple
through the projection provider, for the rows a consumer asks for (see
[runtime.md](runtime.md)). The module registers its `CustomScanMethods`
with `RegisterCustomScanMethods` in `_PG_init`, so that a parallel worker
can read the plan back. Paths are built with `tess_path_create` and plans with
`tess_plan_create` (see [runtime.md](runtime.md)), which is how another
node recognizes a batch child; plan data travels in typed,
`copyObject()`-compatible private lists through the named plan-data codec.

### EXPLAIN and EXPLAIN ANALYZE

`ExplainCustomScan` reports the common properties, the batch size, the
number of batches, the rows read from the source and the rows filtered, and
the node's own. One `ExecProcNode` call returns a whole batch of N rows, but
the executor's instrumentation counts one tuple per call. The shared output
helper adds `rows - 1` to `instrument->tuplecount` when it publishes a batch
and counts removed rows with `InstrCountFiltered1`; a node never adjusts
instrumentation itself, so `EXPLAIN ANALYZE` row counts stay right in every
node.

### Rescan

`ReScanCustomScan` runs in one fixed order: clear the node's output,
releasing an active batch; finish the node's input; rescan the children
with `ExecReScan`; reset the input, which forgets cached slot and binding
pointers; then reset the node's counters and state. Nothing is restored:
cleared selection bits never come back, and a new scan starts from a fresh
batch. End and rescan paths both clear outputs.

### Parameters and paths

Batch paths are never parameterized: planning rejects relations with
`lateral_relids` and paths with `param_info`, and the runtime's path helper
refuses a parameterized template, so a batch node is never the inner side
of a nested loop. Execution `Param`s reach a node only as scalars
inside compiled expressions, which the int4 expression compiler evaluates
through `ExprState`; a changed parameter affects the node only through
rescan.

### Executor flags

A node declares neither backward scan nor mark/restore: the path flags
`CUSTOMPATH_SUPPORT_BACKWARD_SCAN` and `CUSTOMPATH_SUPPORT_MARK_RESTORE`
stay unset. Scrollable cursors and merge joins still work: the planner puts
a `Material` node above the batch subtree, or above the inner side of the
join, and `Material` strips those flags from its child, so a batch node
never sees `EXEC_FLAG_BACKWARD` or `EXEC_FLAG_MARK`. A node still checks the
two flags in `BeginCustomScan` and raises `ERROR` on them, as a guard against
a planner defect. `EXEC_FLAG_REWIND` is supported through rescan.
`ExecSetTupleBound` is optional and up to the node; a limit node calls the
unary helper's form of it, which reaches a batch child through the node
kind's `set_tuple_bound` callback, and a pass-through node like the pack
node forwards the bound and pulls no more rows than it. In the same way a
parent may hand a batch child a key filter (`TessKeyFilter`: key columns
of the child's batches and their kinds, and a Bloom filter of hashes)
through the kind's optional `set_key_filter` callback, which returns
whether the node takes it: a node that does applies it to the batches it
returns from then on, until the parent takes it back with `NULL`, and
the filter stays the parent's. A hash join hands its Bloom filter to a
TessFilter below it this way.

### Parallel execution

This contract does not restrict parallel execution; it requires honest
declarations. A node offers a partial path built from the core's partial
path as its template, so that `parallel_safe` and the number of workers
are the core's and the rows are one participant's share; a node without
shared state above a parallel-safe child is parallel-safe. `parallel_aware`
is set only on a path whose node installs the full set of shared memory
callbacks (`EstimateDSMCustomScan`, `InitializeDSMCustomScan`,
`ReInitializeDSMCustomScan`, `InitializeWorkerCustomScan`,
`ShutdownCustomScan`), whether for work it divides among the participants,
as the heap scan's shared page handout, or only for the counters it sums
over them through `TessSharedStats` ([runtime.md](runtime.md)), since
PostgreSQL calls those callbacks of a custom scan only then; a node above
a parallel child that installs none clears the flag, as the pack node
does. Shared state exists only in DSM or DSA, in the node's chunk keyed by
its plan node id. Objects that refer to DSM are released from
`ShutdownCustomScan`, while the mapping still exists, and nothing a serial
run still needs is ended there, since the executor shuts a plan down after
a partial run of it too; `EndCustomScan` is only a fallback for execution
without DSM. A node begun in the callbacks falls back to serial work when
it is executed without them. A parallel batch subtree runs under one
`Gather` and contains no scalar boundary between batch nodes. The model
is pg_batch's parallel hash join: immutable column-major build chunks in
DSA, a shared atomic bucket array filled with compare-and-swap, and a spill
with one file namespace per participant and partition, followed by a
barrier and single-owner partition claims. Structures in shared memory that
Rust maintains follow rules of their own: memory allocated by C, offsets
instead of pointers, atomics through raw pointers, no allocation in Rust,
with the protocols written in Rust and checked under loom, their phases
included; the hash table
of joins and grouping ([table guide](table.md)) is built that way.

### The enable switch

The bridge defines the GUC `tessera.enable`, boolean and on by default, and
publishes its value through `api->settings->enable`. Every hook first calls
the hook it replaced, then checks the switch, and only then adds paths. A
node module never defines `tessera.enable` itself: several independent
modules install hooks, and a GUC can be defined once per backend. Other
GUCs are added only with a measurement that justifies them; the module
that defines the last of them reserves the prefix (`tessera_nodes`), so
values given before it loads are kept.

### Memory

State that lives for the whole scan is allocated in `estate->es_query_cxt`.
Buffers that live for one batch, such as a builder's copies of
pass-by-reference values or row copies, live in a context of their own that
is reset per batch. Expression contexts are reset per batch, not per row.
The bridge keeps a binding in the slot's `tts_mcxt` with a reset callback;
external resources need their own memory-context callback or `ResourceOwner`
protection, because the bridge's reset only removes its lookup entry. The
row-wise path allocates nothing.

### Row-wise parents

A node either serves a parent that is not batch-aware, through the shared
output helper's row view of one selected row per call, or rejects such a
plan while planning. In the first chain the aggregate node serves a row-wise
parent, publishing its one result row as a batch the helper serves, and so
does the filter node: a hook on a base relation cannot see the parent, and
the unary helper serves rows for free. The heap scan node serves them the
same way, the columns of its targets taken once per batch. The pack node
rejects one, as pg_batch's pack node does; a batch parent always creates it.

### Errors

A node reports errors with `ereport`. Statuses returned by the Rust kernels
become `ereport` after the entry point has returned (see
[kernels.md](kernels.md)); the bridge raises `ERROR` itself for invalid
operations. No Rust frame is on the stack when `ERROR` is raised.

### Tests of every node

Each node comes with regression tests of a batch-aware parent and a
row-wise parent, or the planning-time rejection of one; an empty selection;
batches of fewer and of more than 64 rows; rescan; an early stop by a limit
above the node; NULL values; `EXPLAIN ANALYZE` with correct row counts; a
parallel plan with two workers, when the node offers a partial path; and
lifecycle errors. Hot paths are benchmarked against pg_batch where a
counterpart exists.

### Checklist

- Publish batches through the shared output helper; never touch
  instrumentation directly.
- Register `CustomScanMethods` in `_PG_init`; build paths and plans through
  the runtime helpers.
- Bind `ps_ResultTupleSlot` in `BeginCustomScan`; read children through the
  shared input helper, or stand on the unary helper when the node only
  removes rows from one child's batches.
- Rescan in the fixed order; clear outputs in both end and rescan paths.
- Leave backward scan and mark/restore undeclared; check the flags in
  `BeginCustomScan`.
- Build partial paths from the core's partial paths; declare
  `parallel_aware` only with the five callbacks, sum counters through
  `TessSharedStats`, and release what refers to DSM from
  `ShutdownCustomScan`.
- Check `*api->settings->enable` in every hook, after calling the previous
  hook.
- Keep per-batch memory in a per-batch context; allocate nothing per row.
- Serve row-wise parents or reject them at planning.
- Raise `ERROR` only after every Rust call has returned.
- Ship the regression tests listed above.
