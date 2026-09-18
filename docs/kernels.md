# Kernels from C

The Rust kernels of `tessera-kernels` reach C through `tessera-capi`, a
static library (`target/<profile>/libtessera_capi.a`) declared in
`include/tessera/kernels.h`. An entry point reads a column as the batch
contract's `TessDatumColumn` (Datum values and NULL flags), reads or narrows
`TessRowMask` bitmaps, and returns a `TessStatusCode` that it also stores in
the caller's `TessStatus`. It never raises `ERROR`: PostgreSQL's error
mechanism jumps over stack frames, and Rust frames must unwind, so the
caller reports the status after the call returns.

## Calling an entry point

```c
#include "tessera/kernels.h"

TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
TessDatumColumn column = TESS_STRUCT_INITIALIZER(TessDatumColumn);

batch->ops->get_datum_column(batch, attno, &batch->rows,
                             TESS_COLUMN_FOR_FILTER, &column);
if (tess_int4_filter(&column, NULL, &batch->rows, TESS_CMP_GT, 0,
                     &status) != TESS_OK)
    ereport(ERROR,
            (errcode(MAKE_SQLSTATE(status.sqlstate[0], status.sqlstate[1],
                                   status.sqlstate[2], status.sqlstate[3],
                                   status.sqlstate[4])),
             errmsg("%s", status.message)));
```

`tess_kernels_abi_version()` must equal `TESS_KERNELS_ABI_VERSION` of the
header the caller was compiled with, and `tess_kernels_layout()` returns the
sizes and offsets the library was built with, for a check at load time.

## Readiness: the `prepared` mask

The batch contract lets a provider leave unrequested rows of a Datum column
uninitialized. Kernels read whole 64-row words on their vector paths, so
they must know which rows are initialized: `prepared` is the mask the column
was obtained with, and `NULL` says the whole column is initialized. Rows a
kernel selects must lie within `prepared`; otherwise the call fails with
`TESS_ERROR_INVALID_ARGUMENT` before reading them. A NULL row within
`prepared` holds a placeholder value, as PostgreSQL slots provide.

## Ownership and aliasing

Entry points borrow everything and own nothing: no buffer is retained,
freed, or allocated. Within one call, a mutable argument (the mask a filter
narrows, a result array, a result mask) must not overlap `prepared`, the
column's arrays, or another mutable argument, and the column must not
change. Every mask carries the column's row count and has no bits set
beyond it on entry, result masks included (their row bits may hold
anything); a violated rule is an error, not undefined behavior, wherever the
library can detect it (dimensions, null pointers, padding bits), and
undefined behavior where it cannot (overlap, dangling pointers).

## Errors and panics

| Code | SQLSTATE | Meaning |
|---|---|---|
| `TESS_OK` | empty | Success; `message` is empty. |
| `TESS_ERROR_INVALID_ARGUMENT` | `XX000` | Dimensions, pointers, masks or the operation are invalid, or a selected row is unprepared. |
| `TESS_ERROR_INTEGER_OUT_OF_RANGE` | `22003` | An int4 result does not fit. |
| `TESS_ERROR_DIVISION_BY_ZERO` | `22012` | A zero divisor. |
| `TESS_ERROR_PANIC` | `XX000` | A Rust panic was caught; `message` holds its text. |

A `NULL` or undersized status (its `struct_size` below
`TESS_STATUS_MIN_SIZE`) is left alone; the return value still carries the
code. Messages longer than the buffer are cut on a character boundary.

After a failure the mutable outputs of the call are unspecified: a mask
partly narrowed by a filter, a result array partly written. The caller must
not use them; in practice it raises `ERROR`, which discards the batch.
Results returned through plain pointers (an aggregate's value) are written
only on success.

A panic inside a kernel is caught at the entry point and reported as
`TESS_ERROR_PANIC`; the library keeps no state across calls, so later calls
work normally. `tess_kernels_test_panic()` raises one on purpose, and the C
test module `test/tessera_kernels_test.c` calls it with both the debug and
the release library to verify that the linked library unwinds rather than
aborts. The library never calls PostgreSQL, so an `ERROR` cannot arise
inside a kernel.

## Entry points

- `tess_int4_filter(column, prepared, rows, op, scalar, status)`: keep in
  `rows` the selected rows whose non-NULL value satisfies `value op scalar`;
  NULL never satisfies a comparison.
- `tess_int4_count(column, prepared, rows, count, status)`: the number of
  selected non-NULL values, as `count(column)`.
- `tess_int4_sum(column, prepared, rows, isnull, sum, status)`: the int8
  sum of the selected non-NULL values, NULL without any; one batch's sum
  cannot overflow, the caller checks the running total across batches.
- `tess_int4_min` and `tess_int4_max(column, prepared, rows, isnull, value,
  status)`: the least or greatest selected non-NULL value, NULL without any.
- `tess_int4_arith_scalar(op, column, scalar, prepared, rows, values,
  non_nulls, status)`, `tess_int4_arith_scalar_left(op, scalar, column, …)`
  and `tess_int4_arith_columns(op, left, left_prepared, right,
  right_prepared, rows, values, non_nulls, status)`: `+`, `-`, `*`, `/`
  and `%` with PostgreSQL's semantics into a dense int4 result. For every
  word with selected rows, `non_nulls` receives the selected rows whose
  operands are non-NULL and `values` their results; other rows of `values`
  are unspecified and need no initialization, and words without selected
  rows get a cleared `non_nulls` word. Overflow reports `22003`, a zero
  divisor `22012`; `MIN / -1` is out of range and `x % -1` is 0.
