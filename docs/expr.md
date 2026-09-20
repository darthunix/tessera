# Batch expressions

`tessera/expr.h` compiles a PostgreSQL expression over one batch column into
a chain of calls of the functions the [function registry](function.md)
implements, and evaluates it over a batch's selected rows. It is the
deliberately small language shared by the batch nodes: a filter node
applies the batchable prefix of a scan's qualifiers with it, a projection
computes columns with it, an aggregate feeds its argument through it. There
is no intermediate representation of its own: the expression tree is the
planner's, the operations are the registry's, and the compiler only
arranges the calls in the order the tree dictates.

## The language

An expression is supported when it is built from

- at most one `Var` (of the current relation: `varlevelsup` 0, an ordinary
  attribute, and, when a relation is given, that relation);
- `Const` and `Param` scalars, external or execution parameters;
- calls, `OpExpr` or `FuncExpr`, whose function the registry implements as
  a `TESS_FUNCTION_VALUE` that is strict and either insensitive to the
  input collation or given none, with one or two arguments that are
  themselves supported.

The column must be the first argument of every call that takes it unless
the implementation accepts any shape, or the call is an operator whose
commutator's function is implemented: `100 - a` is fine because the int4
arithmetic accepts any shape; `7 < a` becomes `a > 7`. A scalar argument
may be an expression of its own without a Var; the executor evaluates it
whole. An expression with no Var at all is a scalar broadcast over the
rows. `RelabelType` is transparent. Everything else, `AND`, `OR`, `NOT`,
`CASE`, `COALESCE`, `IS NULL`, a second column, a function the registry
does not know, is left to the row-wise executor. `tess_expr_supports_value`
decides this at planning time, without executor state, with the same rules
the compiler enforces.

## Compiling and evaluating

A node compiles its expressions in `BeginCustomScan`, in the query's memory
context, with a callback that maps a Var to a batch column of the node's
layout:

```c
static int
resolve_column(const Var *var, void *context)
{
    return tess_layout_column(layout, var->varattno - 1);
}

expr = tess_expr_compile_value(node, &css->ss.ps, resolve_column, layout);
column = tess_expr_input_column(expr);   /* -1 for a scalar expression */
```

Per batch, the node binds the expression and asks for the results:

```c
tess_expr_bind(expr, batch, econtext, TESS_COLUMN_FOR_PROJECTION);
column = tess_expr_get_column(expr);
non_nulls = tess_expr_non_nulls(expr);
```

The result is the batch contract's Datum column, one slot per batch row
with NULL flags set for the selected rows, and the mask of the selected
rows whose result is not NULL; both are borrowed until the next bind. The
results are computed once per bind: scalars are evaluated in the expression
context at that point, so a Param changed by a rescan takes effect at the
next batch. The purpose says whether the input column is read while
filtering or for the output, as the batch contract distinguishes.

Evaluation walks the chain from the column outward. Every call takes the
batch's selected rows; a strict function leaves NULL rows out of its
`non_nulls`, and the compiler turns that mask into the NULL flags of the
next call's column, so NULL flows through the chain as it does through the
row-wise executor. A NULL scalar makes the whole step NULL without a call.
Implementations that return `TESS_RESULT_INT32` write int32 results, which
the compiler widens into the Datum column between steps; that costs one
pass over the selected rows per step, and native column formats will remove
it later. A failed call raises its SQLSTATE and message after the call has
returned, as every kernel error is reported.

## Errors

An unsupported expression, a Var the resolver cannot map, and a result
request before any bind are errors at compile or first use. A division by
zero or an out-of-range result surfaces as the function's own error,
`22012` or `22003`, at evaluation.
