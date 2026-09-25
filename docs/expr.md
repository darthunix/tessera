# Batch expressions

`tessera/expr.h` compiles a PostgreSQL expression over batch columns into
chains of calls of the functions the [function registry](function.md)
implements, and evaluates it over a batch's selected rows. It is the
deliberately small language shared by the batch nodes: `TessFilter`
([nodes.md](nodes.md)) applies the batchable clauses of a relation with it, a projection computes columns with it, an aggregate feeds its
argument through it. There
is no intermediate representation of its own: the expression tree is the
planner's, the operations are the registry's, and the compiler only
arranges the calls in the order the tree dictates.

## The language

An expression is supported when it is built from

- `Var`s (of the current relation: `varlevelsup` 0, an ordinary
  attribute, and, when a relation is given, that relation): one is the
  column the chain starts from, and a call whose implementation accepts
  any shape may take a second value over the batch as its other argument,
  the operand, which is a supported expression of its own: a bare column
  (`a + b`, `(a + 1) * b`, `b - a`) or a computed one (`(a + 1) * (b + 2)`,
  `a - b * 2`, `(a + 1) * (b + a * 2)`), so an expression is a tree;
- `Const` and `Param` scalars, external or execution parameters;
- calls, `OpExpr` or `FuncExpr`, whose function the registry implements as
  a `TESS_FUNCTION_VALUE` that is strict and either insensitive to the
  input collation or given none, with one or two arguments that are
  themselves supported;
- calls of an equivalent ([function.md](function.md)), such as
  `int48pl(a, b8)`: the compiler replaces the call by the function it
  stands for over the cast arguments, `int8pl(int8(a), b8)`, before
  anything else, so the cast of a column is a step of its own and the cast
  of a constant is folded once. The replacement has no operator, so its
  function must accept the column where it lands, as the int8 functions,
  of any shape, do.

The column must be the first argument of every call that takes it unless
the implementation accepts any shape, or the call is an operator whose
commutator's function is implemented: `100 - a` is fine because the int4
and int8 arithmetic accept any shape; `7 < a` becomes `a > 7`. A scalar argument
may be an expression of its own without a Var; the executor evaluates it
whole. An expression with no Var at all is a scalar broadcast over the
rows. `RelabelType` is transparent. Everything else in a value, `CASE`,
`COALESCE`, a function the registry does not know,
anywhere in the tree, is left to the row-wise executor for the whole
expression. `tess_expr_supports_value`
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
rows whose result is not NULL; both are borrowed until the next bind. For
a bare column the mask is built when first asked for, since a filter over
the column never needs it. The results are computed once per bind: scalars are evaluated in the expression
context at that point and broadcast over the batch's rows word by word, so a
Param changed by a rescan takes effect at the next batch. The purpose says whether the input column is read while
filtering or for the output, as the batch contract distinguishes.

Evaluation walks the chain from the column outward; a step with an
operand first computes the operand, a chain of its own with its own
scratch arrays, over the same selected rows (a bare column is read from
the batch without a copy), then calls the implementation over the two
columns. Every call takes the
batch's selected rows; a strict function leaves NULL rows out of its
`non_nulls`, and the compiler turns that mask into the NULL flags of the
next call's column, so NULL flows through the chain as it does through the
row-wise executor. A NULL scalar makes the whole step NULL without a call.
Implementations that return `TESS_RESULT_INT32` write int32 results, which
the compiler widens into the Datum column between steps; that pass goes
word by word over the batch's rows, writing every row of a word with
selected rows, so that it vectorizes, and native column formats would
remove it altogether. The int8 kernels return `TESS_RESULT_DATUM` and write
their int64 results into the Datum column directly, since an int8 is its
Datum; the cast `int8(int4)` is such a step, so `c4::bigint * 3 > 570` is a
chain of the cast, the mixed multiplication and the mixed comparison. A failed call raises its SQLSTATE and message after the call has
returned, as every kernel error is reported.

## Filters

A filter is a boolean call of a function the registry implements as a
`TESS_FUNCTION_PREDICATE`, over one supported value with the column and
either one scalar or another value over the batch, the operand: `a > 5`,
`a + 1 > 5`, `7 < a`, `a > b`, `a > b * 20`, `a + 1 > b * 2`. An operand
needs a predicate of any shape, as the built-in comparisons are; the
operand is computed over the same selection and a NULL on either side
clears the row. `tess_expr_supports_filter`
recognizes it at planning time; `tess_expr_compile_filter` compiles the
value chain and the predicate; and per batch, after a bind,
`tess_expr_apply_filter` narrows the batch's row mask in place to the
selected rows where the predicate is true:

```c
tess_expr_bind(filter, batch, econtext, TESS_COLUMN_FOR_FILTER);
tess_expr_apply_filter(filter);
```

The value is computed over the selection as it stands, the predicate
clears the rows where it is false or the value is NULL, and a NULL scalar
clears every selected row, as a strict predicate would. Filters compose by
applying one after another to the same batch; each sees the rows the
previous ones kept. A predicate that does not accept any shape takes the
column first, so `7 < a` is compiled through the operator's commutator
from the catalog into `a > 7`; without a commutator whose function is
implemented, the filter is not supported. What a filter cannot express,
a boolean column, stays with `ExecQual` over the rows that survive.

## Conditions

A filter may also be a condition of several parts, in SQL's three-valued
logic: `AND`, `OR` and `NOT` over supported conditions; `IS NULL` and `IS
NOT NULL` over a supported value, a bare column of any type among them,
since only its NULL flags are read; `IS [NOT] TRUE`, `IS [NOT] FALSE` and
`IS [NOT] UNKNOWN` over a supported condition; and `x op ANY (array)` or
`x op ALL (array)` over a constant array of at most 32 elements, so `a IN
(1, 3, 5)` and `a NOT IN (1, NULL)` are covered: `x` is computed once over
the selection, as the executor computes it once per row, and the
predicate compared against each element, for `ANY` over the rows no
earlier element matched, for `ALL` over the rows every earlier one did;
a NULL element leaves a row that matches no other unknown for `ANY` and
makes every row that is not false unknown for `ALL`, as the array
operators say. A cross-type operator in the list stands for its
equivalent, the elements cast once. A condition evaluates to
two masks over the selection it is given, the rows where it is true and
those where it is unknown (NULL), the rest being false: a leaf's unknown
rows are those where its value, its operand or a scalar is NULL, and the
connectives combine the masks word by word, with no kernel. A child runs
only over the rows the executor would evaluate it for: the right side of
an `OR` where the left one is not true, of an `AND` where the left one is
not false, so `b = 0 OR 10 / b > 1` divides no row by zero, and `a = 3 AND
10 / (a - 3) > 1` fails on the row where `a` is 3, as in the executor.
The unknown mask is computed only where a `NOT`, a boolean test or an
inner `AND` needs it. A boolean column (`WHERE flag`) waits for the type.

## Errors

An unsupported expression, a Var the resolver cannot map, a result request
before any bind and a filter applied through a value expression are errors
at compile or first use. A division by zero or an out-of-range result
surfaces as the function's own error, `22012` or `22003`, at evaluation.
The rows that fail are the executor's: a strict function's arguments are
evaluated for every row it sees, here over every selected row. With
failures on different rows the error reported may differ, since the
compiler computes one call over the whole batch before the next, and the
executor one row after another.
