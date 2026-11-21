# Batch sources

A batch source is a module that reads a relation Tessera's heap scan
node does not read, a foreign table today, in batches through a node of
its own, and may check some of the relation's clauses itself. A source
registers in the source registry; Tessera's scan planner asks the
sources in turn whether they read a foreign table, and plans
`TessFilter` and the batch parents over the path the source returns.
The promises are the capability `source-scan`'s spec; this guide shows
how a source keeps them. The test source of the suite `source_scan`,
`test/tessera_scan_source_test.c`, is a complete example: a foreign data
wrapper, a batch node and sources that read, decline or fail on purpose.

## Finding the registry

Get the registry once during module initialization, check its table
with `tess_source_registry_check` (`tessera/runtime_api.h`), and keep
it:

```c
const TessSourceRegistryOps *sources =
    tess_source_registry_check(tess_runtime_api()->sources);
```

The check raises `0A000` for a table of another version, one without
`next`, or `NULL`. A module that does not link the runtime library
checks `abi_version` against `TESS_SOURCE_REGISTRY_OPS_ABI_VERSION` and
`struct_size` against `TESS_SOURCE_REGISTRY_OPS_MIN_SIZE` itself, before
it calls `next`.

## Registration

The provider defines a source with static storage and registers it:

```c
static const TessSource my_source = {
    TESS_ABI_INITIALIZER(TESS_SOURCE_ABI_VERSION, TessSource),
    .name = "my_extension.parquet",
    .scan_rows = my_scan_rows,
};

sources->add(&my_source);
```

Names are case-sensitive and must be unique in one PostgreSQL backend.
Adding the same pointer again is safe. Adding another source with the
same name is an error, `42710`; a `NULL` source or one without a name
is `XX000`, another version or a size below `TESS_SOURCE_MIN_SIZE`
`0A000`. A source of the least size, which ends at `name`, has no
`scan_rows` and reads nothing; `scan_rows` set to `NULL` means the same.

`sources->next(NULL)` gives the first source registered, `next(source)`
the one after it, `NULL` after the last: the scan planner asks them in
this order, and the first that reads a relation wins. `next` refuses a
pointer that is not registered, `XX000`.

## Ownership and lifetime

The provider owns the `TessSource` structure and the string referenced
by `name`. The registry stores the provider's pointer without copying
the structure or name string. The structure and name string must stay
valid and unchanged from registration until `sources->remove(&my_source)`
returns. Removing the same source more than once is safe while the
object is still alive; removing `NULL` is also safe.

`sources->find("my_extension.parquet")` returns the registered pointer
or `NULL`; so do `next`'s results. The returned pointer is borrowed: the
consumer must not modify or free the source or its name, and must not
use it after removal. Before removing a source, the provider must
ensure that all consumers have finished using their borrowed pointers.
`remove` only unregisters the source; it does not wait for consumers or
free the structure or its name.

A dynamically allocated source lives in a memory context that stays
alive until removal:

```c
TessSource *source = palloc(sizeof(*source));
char *name = pstrdup("my_extension.dynamic_scan");

*source = (TessSource) {
    TESS_ABI_INITIALIZER(TESS_SOURCE_ABI_VERSION, TessSource),
    .name = name,
};
sources->add(source);

/* After all consumers have finished using their borrowed pointers: */
sources->remove(source);
pfree(name);
pfree(source);
```

## What a source is offered

With `tessera.enable` on, the scan planner offers a foreign table of a
`SELECT` that locks no rows, a base relation, a partition, a child of
inheritance or a branch of a `UNION ALL`, without lateral references
or a pseudoconstant clause. It offers the wrapper's foreign path of
least total cost without parameters or path keys, and, where the
relation may be read in parallel, apart, the one of its parallel-aware
partial paths: each a call of its own with answers of its own. A heap
table never reaches a source, and no batch parent asks one.

```c
static CustomPath *
my_scan_rows(PlannerInfo *root, Path *path, TessScanClauses *clauses)
{
    if (!IsA(path, ForeignPath) ||
        path->parent->fdwroutine->GetForeignPaths != my_get_foreign_paths)
        return NULL;
    ...
}
```

`path` is a copy of the core's whole path, `fdw_private` included, so
a source that is also the wrapper finds what its `GetForeignPaths`
planned. The copy is Tessera's and lives as long as the planning of the
statement: read it, do not change it, and do not make it a child of
your path. Its target holds the relation's targets and every column the
clauses read.

## Answering for the clauses

`clauses` holds the relation's clauses in the order the core evaluates
them, `usable[i]` whether the source may use clause `i`, and
`answers[i]`, `TESS_CLAUSE_UNSUPPORTED` on entry, for the source to set:

- `TESS_CLAUSE_UNSUPPORTED`: the source does not use the clause, and
  `TessFilter` checks it.
- `TESS_CLAUSE_PRUNE_ONLY`: the source may skip rows the clause
  rejects, a group of rows whose least and greatest values it rejects,
  say; `TessFilter` still checks it on the rows that reach it.
- `TESS_CLAUSE_EXACT`: every row the source returns passes it, and
  `TessFilter` does not check it.

Use only a clause marked usable. Tessera marks a clause usable when it
calls no volatile function, holds no `SubPlan`, and is leakproof or of
the relation's lowest security level: a source that uses a clause runs
it ahead of the clauses `TessFilter` keeps, a row security policy's
among them. Any other answer for a clause not usable, a value that is
none of the three, or an answer for a clause whose expression the path
does not carry is `XX000`.

A clause the source checks by code of its own, rather than through
PostgreSQL's functions, raises no error and writes no message that
depends on the rows' values: the rows it sees may be ones a policy or
a view's clause will hide. When it cannot promise that, it answers
`TESS_CLAUSE_UNSUPPORTED`.

## The path to return

Return `NULL` when the source does not read the relation, and Tessera
ignores what it wrote into the answers. Otherwise build a new path with
`tess_path_create` ([runtime.md](runtime.md), "Building paths"), from the
copy as its template, for a node of the node registry:

```c
TessPathConfig config = TESS_STRUCT_INITIALIZER(TessPathConfig);

config.template_path = path;
config.methods = &my_path_methods;
config.node = &my_node;
config.expressions = clauses_used;   /* RestrictInfo.clause of each */
config.node_data = my_plan_data;
config.flags = CUSTOMPATH_SUPPORT_PROJECTION;
scan = tess_path_create(&config);
scan->path.startup_cost = my_startup;
scan->path.total_cost = my_total;
```

Tessera refuses with `XX000` a path that is not a batch path, of another
relation, with parameters or path keys, without projection, serial for a
partial offer or partial for a serial one, a partial one that is not
parallel-aware and parallel-safe with the offer's workers, or a serial
one parallel-safe where the offer is not. The path becomes Tessera's: it
sets the path's target, the offered one under `TessFilter` and the
relation's own when the path stands alone, and, when it stands alone,
its rows, and `add_path` may free it, so build a new one at each call
and keep no pointer to it.

The path costs what the source says, per participant for a partial path:
Tessera neither scales it by `tessera.scan_cost_factor`, which
calibrates the heap scan, nor adds anything to a path alone. Under
`TessFilter` the path keeps its rows, the source's estimate of the rows
it returns, which the filter reads; the path alone and the filter over
it take the offered path's rows, what the relation returns. `TessFilter`
over the path adds the cost of the clauses it keeps, as the core counts
it.

## The plan

`PlanCustomPath` builds the plan with `tess_plan_create`, the clauses
the source uses as its expressions, no qualifiers and the relation as
the scan tuple:

```c
TessPathInfo info = TESS_STRUCT_INITIALIZER(TessPathInfo);
TessPlanConfig config = TESS_STRUCT_INITIALIZER(TessPlanConfig);
TessLayout layout = TESS_STRUCT_INITIALIZER(TessLayout);

tess_path_get_info(best_path, &info);
layout.ncolumns = rel->max_attr;
layout.ntargets = rel->max_attr;
config.methods = &my_scan_methods;
config.layout_policy = TESS_LAYOUT_PROJECTED;
config.explicit_layout = &layout;
config.expressions = info.expressions;
config.node_data = info.node_data;
config.scanrelid = rel->relid;
config.scan_tuple_is_relation = true;
return tess_plan_create(best_path, tlist, custom_plans, &config);
```

PostgreSQL hands `PlanCustomPath` every clause of the relation, those
the source leaves and those it may not use among them: leave them out
of the plan's qualifiers, the conditions a node checks on each row it
returns, which `EXPLAIN` shows as its `Filter`; a node that checked
them would run them all, ahead of `TessFilter` and twice. The
expressions become the plan's `custom_exprs`, whose parameters the
planner lists for the node, so that a new value of an outer row's or a
subquery's parameter starts its scan again, and which the planner's
last pass resolves against the relation. `TessFilter`
refuses with `XX000` a source plan under it with qualifiers, or without
a clause the source answered for among its expressions.

## Executing

The node is an ordinary batch node ([node.md](node.md)): it publishes
batches through the output helper, serves a row-wise parent, rescans,
and installs the shared memory callbacks of a parallel-aware path to
divide the rows among the participants. A clause may compare a column
with a value that is not a constant of the plan: an outer row's value,
a prepared statement's parameter, an uncorrelated subquery's value, or
an expression such as `now()`. Compute that side through PostgreSQL's
evaluation of expressions in the node's context when the node is first
asked for rows and again after each rescan, never when it is begun:

```c
state->side = ExecInitExpr(side_expr, &css->ss.ps);       /* begin */
...
value = ExecEvalExprSwitchContext(state->side,             /* first rows */
                                  css->ss.ps.ps_ExprContext, &isnull);
```

The evaluation computes a subquery's value when it is first read and,
in a worker, reads the value the gathering node sent; a plain `EXPLAIN`
runs nothing. The node shows in `EXPLAIN` the clauses it checks
exactly; rows it skipped or removed count in no line of `TessFilter`.
