# Nodes module

`tessera_nodes` (`nodes/`) is the module of Tessera's own batch nodes. It
links the runtime library statically and, when loaded, registers its node
kinds in the bridge's node registry and its scan methods with PostgreSQL.
It installs no planner hook of its own: the pack node below is created by
batch parents, and the nodes that add paths themselves come later. It is
loaded after the bridge; loading it without the bridge is an error. A
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

## TessPack

`TessPack` turns the rows of an ordinary child into batches. It is the
boundary between PostgreSQL's row-wise plans and Tessera's batch nodes, so
that a batch parent can stand above any core path without a patch to
PostgreSQL; a scan that produces batches natively replaces it later.

### Planning

A parent never creates the pack path directly. It asks
`tess_batch_input_path` for a batch child over the path it has, and the
helper calls the `wrap_rows` callback the pack node registers under
`tessera.pack`. The pack path copies its child's planner properties: rows,
costs, path keys and parallel safety. There is no cost model yet, so the
path costs exactly what its child costs. The pack path exists only as a
child of a batch parent; the module adds it to no path list.

`PlanCustomPath` builds the scan with the dense layout, the child's target
list as the scan target list and no qualifier of its own: the child was
planned with its exact target list, so child attribute N is target N, and
the relation's clauses are already evaluated by the child. A pack path is
never parameterized, since the path helper refuses such a template.

### Execution

`BeginCustomScan` reads the plan, initializes the child and binds the
result slot through the output helper. The builder is created at the first
execution, once the parent's request is frozen: PostgreSQL initializes the
plan top-down, so the parent sends its request only after the pack node's
`BeginCustomScan` has bound the slot. The batch size is the request's
`max_batch_rows` capped at 64 rows, or 64 when the request leaves it open.
A request for rows is an error: the pack node serves batch-aware parents
only, and a row-wise parent never plans one.

Each execution first returns the previous batch to the builder, which is an
error while the parent has not finished it, then fills up to the batch size
from the child and publishes the batch; once the child is exhausted, it
publishes the remaining rows and afterwards returns nothing. Every column
is materialized: the request's column masks are not used until lazy
projection arrives. Rescan clears the output, rescans the child and starts
a fresh batch. The node is parallel-safe whenever its child is and keeps no
shared state. When the parent bounds the rows it needs, through the node
kind's `set_tuple_bound` callback, the pack node forwards the bound to its
child and pulls no more rows than that, so a sort below stays a top-N sort
and the last batch may be short.

`EXPLAIN` shows `Batch Size` once the node has executed, since the size
follows the parent's request, and with `ANALYZE` the number of `Batches`;
the row counts are corrected by the output helper, so the node reports the
rows it packed.

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
