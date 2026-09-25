# Batch functions

A `TessFunction` describes a batch implementation of one PostgreSQL function:
a callback that computes the function over a whole batch, with the same
values and the same errors as the function itself. Consumers such as the
[expression compiler](expr.md) look implementations up by the function's OID, the
`funcid` of a `FuncExpr` or the `opfuncid` of an `OpExpr`, and evaluate
whatever they find over the batch; everything else stays row by row through
`ExecEvalExpr`. The registry is a subsystem of the bridge (`TessApi.functions`,
`include/tessera/function.h`), so implementations from independent modules
meet consumers from other modules.

## Registering and finding

After the bridge is loaded, a provider validates the registry table and
registers descriptions with static storage:

```c
static TessStatusCode int4pl_batch(TessFunctionCall *call);

static const TessFunction int4pl = {
    TESS_ABI_INITIALIZER(TESS_FUNCTION_ABI_VERSION, TessFunction),
    .funcid = F_INT4PL,
    .kind = TESS_FUNCTION_VALUE,
    .result_format = TESS_RESULT_INT32,
    .flags = TESS_FUNCTION_STRICT | TESS_FUNCTION_COLLATION_INSENSITIVE |
             TESS_FUNCTION_ANY_SHAPE,
    .evaluate = int4pl_batch,
};

api->functions->add(&int4pl);
```

`find(funcid)` returns the registered description or `NULL`. As for nodes
and sources, the registry stores the pointer without copying, `add` of the
same pointer again is safe, another description for the same function is an
error, `remove` unregisters only that exact object without waiting for
consumers, and a pointer returned by `find` is borrowed. An invalid
description is rejected: a wrong ABI version or size, an invalid `funcid`, a
missing `evaluate`, an unknown kind or result format, or a non-strict
function.

Built-in integer functions are registered by the kernels module
`tessera_kernels` (`kernels/`), which links the Rust kernels statically and
registers, when loaded: for int4, `int4eq`, `int4ne`, `int4lt`, `int4le`,
`int4gt`, `int4ge` as predicates of any shape (a column with a scalar on
either side, or two columns), `int4pl`, `int4mi`, `int4mul`, `int4div`,
`int4mod` and the unary `int4um` as values with `TESS_RESULT_INT32`; for
int8, the six comparisons `int8eq` … `int8ge`, the six of a bigint column
with an integer scalar (`int84eq` … `int84ge`, the scalar widened), the
six of an integer column with a bigint scalar (`int48eq` … `int48ge`, a
scalar within the int4 range compared as int4, one beyond it a constant
answer for every non-NULL value; also what `100 < c8` commutes into),
`int8pl`, `int8mi`, `int8mul`, `int8div`, `int8mod` in every shape and
`int84pl`, `int84mi`, `int84mul`, `int84div` with the column first, the
unary `int8um`, and the cast `int8(int4)`, all as values with
`TESS_RESULT_DATUM`, since an int8 is its Datum; and `count(*)`,
`count(any)`, `sum(int4)`, `min(int4)`, `max(int4)`, `min(int8)` and
`max(int8)` as aggregates, by the aggregate's own OID. The mixed arithmetic
with the integer on the left (`int48pl` and its siblings) is left to the
core: an int8 kernel must not read an int4 Datum as a whole word. The
module also installs the key hashes and the hash table in the bridge's
kernel registry, for nodes that do not link the kernels (see
[bridge.md](bridge.md)). Load the bridge first, then `LOAD 'tessera_kernels'`, or preload both through
`session_preload_libraries` as [bridge.md](bridge.md) recommends. The
description does not depend on the argument types, so implementations for
other types and from other extensions use the same structure.

## What a description promises

- **Equivalence.** For every input the implementation returns what the
  function with OID `funcid` returns and fails where it fails, reporting the
  function's SQLSTATE in the call's status. The argument and result types
  are the function's own, from `pg_proc`; the description does not repeat
  them, and a consumer checks them against the expression.
- **Strictness.** `TESS_FUNCTION_STRICT` is required in this version: a NULL
  argument makes a NULL result, or a false predicate. The consumer folds NULL
  scalars before calling, so an implementation never sees one.
- **Collation.** `TESS_FUNCTION_COLLATION_INSENSITIVE` says the input
  collation cannot change the result. Without it the consumer passes
  `inputcollid` and the implementation must honor it or fail.
- **Shape.** With `TESS_FUNCTION_ANY_SHAPE` every combination of column and
  scalar arguments is accepted. Without it, `args[0]` is the only column and
  the others are scalars: the consumer moves the column first through the
  operator's commutator from the catalog (`get_commutator`, then
  `get_opcode` and `find`), so that `7 < x` evaluates as `x > 7`.

## Calling

A `TessFunctionCall` carries the arguments, each a `TessDatumColumn` of the
batch with its readiness mask or a scalar, the selection, and the outputs of
the function's kind:

- `TESS_FUNCTION_PREDICATE` narrows `rows` in place to the selected rows
  where the function is true.
- `TESS_FUNCTION_VALUE` writes the results of the selected non-NULL rows
  into `values`, an array of one slot per batch row in the description's
  `result_format`, and names those rows in `non_nulls`; other slots are
  unspecified and need no initialization. With `TESS_RESULT_DATUM`,
  by-reference values are allocated in the call's `context`.
- `TESS_FUNCTION_AGGREGATE` computes a partial aggregate over the selected
  rows of the batch, with no argument for `count(*)` or one column: one
  Datum of the aggregate's transition type into `values`, and the only bit
  of `non_nulls`, a mask of one row, when the partial is not NULL, which
  `sum`, `min` and `max` are without a contributing row. Strict means that
  NULL rows are left out, as with a strict transition function. The
  consumer combines the partials of the batches itself, checking overflow
  where PostgreSQL would.

The columns are the batch contract's mandatory Datum representation; the
kernels read it directly, and native column interfaces will extend
`TessFunctionArg` when a producer offers them. Implementations return the
code they also store in `status`, never raising `ERROR`; the consumer
reports a failure with `ereport` after the call returns (see
[kernels.md](kernels.md)). After a failure the mutable outputs are
unspecified.

## Functions of other extensions

A later version will let an extension attach a batch implementation to its
own function through PostgreSQL's planner support functions
(`CREATE FUNCTION ... SUPPORT`): `find` will ask the function's support
function with an `ExtensibleNode` request named `tessera.batch_function`,
carrying the request's ABI version and size, the `funcid` and the input
collation, and cache the returned `TessFunction` by OID until the function's
catalog entry changes. The description is the same structure as a registered
one.
