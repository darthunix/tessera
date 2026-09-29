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
function; an equivalent (below) shorter than
`TESS_FUNCTION_EQUIVALENT_MIN_SIZE`, without another function to stand for,
or with a callback.

## Equivalents

A function over two types of one family, such as `int48pl(int4, int8)`,
needs no implementation of its own: it returns and raises what `int8pl`
does over its first argument widened. A description of kind
`TESS_FUNCTION_EQUIVALENT` says so as data: `equivalent` is the function to
call instead, and `arg_casts` holds, per argument, a one-argument cast to
apply first, or `InvalidOid`:

```c
static const TessFunction int48pl = {
    TESS_ABI_INITIALIZER(TESS_FUNCTION_ABI_VERSION, TessFunction),
    .funcid = F_INT48PL,
    .kind = TESS_FUNCTION_EQUIVALENT,
    .flags = TESS_FUNCTION_STRICT | TESS_FUNCTION_COLLATION_INSENSITIVE,
    .equivalent = F_INT8PL,
    .arg_casts = {F_INT8_INT4, InvalidOid},
};
```

The consumer compiles the equivalent function over the cast arguments, a
cast of a column being a step of its own and a cast of a constant folded
once; so a family of k types needs k sets of kernels and the casts between
them, and each pair of types only lines of data. The promise is the
provider's: only casts that lose nothing (widenings) keep the results and
the errors the same. The equivalent and the casts are looked up when an
expression is compiled, so they may be registered in any order, and an
expression whose equivalent or casts have no implementation stays row by
row.

Built-in integer functions are registered by the kernels module
`tessera_kernels` (`kernels/`), which links the Rust kernels statically and
registers, when loaded: for int4, `int4eq`, `int4ne`, `int4lt`, `int4le`,
`int4gt`, `int4ge` as predicates of any shape (a column with a scalar on
either side, or two columns), `int4pl`, `int4mi`, `int4mul`, `int4div`,
`int4mod` and the unary `int4um` as values with `TESS_RESULT_INT32`; for
int8, the six comparisons `int8eq` … `int8ge`, `int8pl`, `int8mi`,
`int8mul`, `int8div`, `int8mod` in every shape, the unary `int8um`, and the
cast `int8(int4)`, all as values with `TESS_RESULT_DATUM`, since an int8
is its Datum; the cast `int4(int8)` as a value with `TESS_RESULT_INT32`,
failing with 22003 past the int4 range; the six comparisons of `date`
(`date_eq` … `date_ge`) as those of int4, and of `timestamp` and
`timestamptz` (`timestamp_eq` …, `timestamptz_eq` …) as those of int8, in
every shape, since those types are their integers in the integers' order,
infinities being the extremes; the functions over an int4 and an int8 (`int84eq` …
`int84ge`, `int48eq` … `int48ge`, `int84pl`, `int84mi`, `int84mul`,
`int84div`, `int48pl`, `int48mi`, `int48mul`, `int48div`) as equivalents of
the int8 function with the int4 argument cast by `int8(int4)`, since an
int8 kernel must not read an int4 Datum as a whole word; the six
comparisons of `boolean` (`booleq` … `boolge`) and of `smallint` against
smallint and integer (`int2eq` …, `int24eq` …, `int42eq` …) as those of
int4, since a boolean is 0 or 1 in its word and a smallint its value
sign-extended; the smallint arithmetic (`int2pl`, `int2mi`, `int2mul`,
`int2div`, `int2mod`, `int2um`) as int4's over those words with the
results checked against the smallint range, 22003 "smallint out of range"
past it, and with an integer (`int24pl` …, `int42pl` …) as int4's; the
casts `int8(int2)` as `int8(int4)`, `int4(int2)` and `int2(int4)`, the
last checked as the smallint arithmetic; the functions of a smallint and a
bigint (`int28eq` …, `int82eq` …, `int28pl` …, `int82pl` …) as equivalents
of the int8 function over `int8(int2)`; the six comparisons of a date
against a timestamp (`date_eq_timestamp` … `date_ge_timestamp`, and
`timestamp_lt_date` and its siblings through their commutators) with a
date column and a timestamp scalar, which becomes the date bound the
comparison keeps once a call, as a date is its days since 2000-01-01
(`d < T` keeps `d < ceil(T / day)`, `d = T` none where `T` is past a
midnight), an infinite timestamp the infinite date and a date past the
timestamps' range above every finite bound, as the core orders them;
the string functions of `text` and `bpchar` (`char(n)`, and `varchar`
through its binary cast to text) with `TESS_FUNCTION_DETERMINISTIC_COLLATION`
as predicates: `texteq`, `textne`, `bpchareq` and `bpcharne` in every
shape, comparing bytes and a bpchar without its trailing spaces;
`starts_with`, `textlike`, `textnlike`, `bpcharlike` and `bpcharnlike`
with a column and a pattern constant, a pattern of literals and `%`
matched by its pieces between the `%`s once split a call, one with `_` or
an escape, or in a multibyte encoding other than UTF-8, by the core's
`textlike` a row; and the lengths `length`, `char_length`,
`character_length` and `octet_length` of text and bpchar as values with
`TESS_RESULT_INT32`, characters counted by the database encoding and a
bpchar's without trailing spaces, `octet_length` from the header without
detoasting; and the pieces of a string as text values (`TESS_RESULT_DATUM`,
`TESS_FUNCTION_COLLATION_INSENSITIVE`) of a column by constants:
`substring` and `substr` from a start, for a length or to the end, `left`
and `right` of either sign, counted in characters as the core counts
them, a negative length failing with 22011 as `substring` does, and
`rtrim`, `ltrim` and `btrim` of spaces, `text(bpchar)` being `rtrim`;
the date functions in every shape: `date_pli`, `date_mii` (a date plus or
minus days, an infinite date kept, 22008 "date out of range" past the
dates) and `date_mi` (the days between, "cannot subtract infinite dates")
as values with `TESS_RESULT_INT32`, the casts `date(timestamp)` (the day,
rounded down) and `timestamp(date)` ("date out of range for timestamp"
past the timestamps), and `date_trunc(text, timestamp)`, whose constant
unit is parsed once a call and applied by calendar arithmetic (a week to
its Monday, the years of a decade, century and millennium as the core
rounds them, "timestamp out of range" below the first timestamp), a unit
that is a column or one it does not know going to the core's
`timestamp_trunc` a row; and `timestamp_pl_interval`,
`timestamp_mi_interval`, `date_pl_interval` and `date_mi_interval`, a
timestamp: the interval's months through the calendar, clamped to the
month's last day, then its days through the Julian day, then its
microseconds, each step checked as the core checks it ("timestamp out of
range", "interval out of range" negating one, "date out of range for
timestamp"), an infinite interval making its infinity; `extract(text,
date)` and `extract(text, timestamp)`, numeric values: the integer fields
of a finite value (day, month, quarter, week, year, decade, century,
millennium, isoyear, dow, isodow, doy, and a date's julian and epoch, a
timestamp's hour, minute and microseconds) by calendar arithmetic, a
timestamp's seconds and milliseconds with their fraction, anything else
(an infinite value, a unit that is a column or one it does not know, a
timestamp's julian and epoch) by the core's function a row, its NULL
for an oscillating field of infinity kept; the numeric comparisons
`numeric_eq` … `numeric_ge` in every shape by the core's `numeric_cmp`,
a call keeping the result for pairs where one side is a cached numeric;
and the casts `numeric(int2)`, `numeric(int4)`, `numeric(int8)`. A
numeric of an integer from -1024 to 4095 comes from a block of the
process made once in `TopMemoryContext`, the same pointer every time, so
years and months neither allocate nor compare twice;
and `count(*)`,
`count(any)`, `sum(int4)`, `min(int4)`, `max(int4)`, `min(int8)` and
`max(int8)` as aggregates, by the aggregate's own OID. An int4 column
against a bigint constant is therefore widened and compared as int8. The
module also installs the key hashes and the hash table in the bridge's
kernel registry, for nodes that do not link the kernels (see
[bridge.md](bridge.md)). Load the bridge first, then `LOAD 'tessera_kernels'`, or preload both through
`shared_preload_libraries` as [bridge.md](bridge.md) recommends. The
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
  collation cannot change the result. `TESS_FUNCTION_DETERMINISTIC_COLLATION`
  says the result is the same under every deterministic collation, where
  equal strings are equal bytes, as for equality and LIKE; the consumer
  keeps a call under a nondeterministic collation away from it. Without
  either, the expression compiler leaves a call under a collation row by
  row; the call still carries `inputcollid`, which an implementation used
  by another consumer must honor or fail.
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
  by-reference values are allocated in the call's `context`, which lives
  until the consumer is done with the results: the expression compiler
  resets it when it binds the expression to the next batch, as long as its
  value arrays live. Scratch memory, such as a detoasted argument, the
  implementation frees itself after the row, wherever it allocated it.
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
