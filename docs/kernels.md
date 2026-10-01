# Kernels from C

The Rust kernels of `tessera-kernels` reach C through `tessera-capi`, a
static library (`target/<profile>/libtessera_capi.a`) declared in
`include/tessera/kernels.h`. An entry point reads a column as the batch
contract's `TessDatumColumn` (Datum values and NULL flags), reads or narrows
`TessRowMask` bitmaps, and returns a `TessStatusCode` that it also stores in
the caller's `TessStatus`. It never raises `ERROR`: PostgreSQL's error
mechanism jumps over stack frames, and Rust frames must unwind, so the
caller reports the status after the call returns.

The same library exports the hash table of joins and grouping, declared
in `include/tessera/table.h` and described in the [table
guide](table.md): its calls take a region of memory the caller owns and
follow the same status rules.

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

Kernels read whole 64-row words on their vector paths, so they must know
which rows are initialized: `prepared` is the mask of the rows that are, and
`NULL` says the whole column is. Rows a kernel selects must lie within
`prepared`; otherwise the call fails with `TESS_ERROR_INVALID_ARGUMENT`
before reading them. A NULL row within `prepared` holds a placeholder value,
as PostgreSQL slots provide. With a mask, a word is taken by the vector path
only when every one of its 64 rows is prepared; other words go row by row
with a readiness check per row, about three times the instructions. The
batch contract (`tessera/batch.h`) has Tessera's providers initialize whole
columns, so the consumers pass `NULL`; the mask is for a provider that
declares uninitialized rows.

## Ownership and aliasing

Entry points borrow everything and own nothing: no buffer is retained,
freed, or allocated (the table's region is the caller's too, handed to
every call). Within one call, a mutable argument (the mask a filter
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
| `TESS_ERROR_INTEGER_OUT_OF_RANGE` | `22003` | An int4 or int8 result does not fit (`integer out of range`, `bigint out of range`). |
| `TESS_ERROR_DIVISION_BY_ZERO` | `22012` | A zero divisor. |
| `TESS_ERROR_PANIC` | `XX000` | A Rust panic was caught; `message` holds its text. |
| `TESS_ERROR_DATA_EXCEPTION` | the function's | Another error of a batch function, as the function it implements raises it (`22011` `negative substring length not allowed`). |
| `TESS_ERROR_DATA_CORRUPTED` | `XX001` | Spilled bytes read back fail their checks (a block's header, a packed body, a chunk of columns): a damaged temporary file. |

One function, `tessera_kernels::error::classify`, tells the errors with an
SQLSTATE of their own (the kernels' arithmetic, calendar and text errors,
which share the trait `SqlError`, and `tessera_spill::Damaged`) from every
other error, which is a misuse of the call, the caller's bug, and comes
back as `TESS_ERROR_INVALID_ARGUMENT` with `XX000`.

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
- `tess_int4_compare_columns(left, left_prepared, right, right_prepared,
  rows, op, status)`: keep in `rows` the selected rows where both columns
  are non-NULL and `left op right`, each column read with its own
  readiness; `tess_int8_compare_columns` compares two int8 columns alike.
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
  divisor `22012`; `MIN / -1` is out of range and `x % -1` is 0. A row with
  a NULL operand never fails, whatever the other operand holds
  (`2147483647 + NULL` is NULL): the row path computes it from the pair
  (0, 1), the whole-word path masks its lanes out of the checks.
- `tess_int4_hash(column, prepared, rows, nulls, hashes, valid, status)`
  and `tess_int4_hash_next(column, prepared, nulls, hashes, valid, status)`:
  the 32-bit key hashes of pg_batch (`murmurhash32`, further keys folded in
  with `hash_combine` in call order). The first key takes the selection and
  sets `valid`; each next key takes `valid` as its selection and narrows
  it, never reading a rejected row. `TESS_NULL_KEYS_REJECT` drops a row
  with a NULL key (joins); `TESS_NULL_KEYS_GROUP` hashes NULL as one fixed
  key (grouping). `hashes` has the batch's row count and any contents;
  rows outside `valid` are unspecified.

The int8 family reads a column whose Datums hold an int8, the whole word
as `DatumGetInt64`, with int64 scalars, values and results; an int8 is its
Datum, so an int8 result column is a Datum column. The two families share
every driver in `tessera_kernels::int`, generic over the lane type (the
choice between whole words and rows, the row loops, the loops over whole
words); each keeps its lane: the Datum's reading, the operations with
their error, and the vector code of a whole word, whose lanes and gaps
differ (plan 4.25):

- `tess_int8_filter(column, prepared, rows, op, scalar, status)`: as
  `tess_int4_filter` over int8 values.
- `tess_count(column, prepared, rows, count, status)`: the number of
  selected non-NULL rows of a column of any type, from its NULL flags
  alone; a count needs no width.
- `tess_int8_min` and `tess_int8_max(column, prepared, rows, isnull, value,
  status)`: the least or greatest selected non-NULL int8 value, NULL
  without any. There is no `tess_int8_sum`: PostgreSQL sums bigint into
  numeric.
- `tess_int8_arith_scalar`, `tess_int8_arith_scalar_left` and
  `tess_int8_arith_columns`: as the int4 ones into a dense int8 result,
  overflow reported as `22003` with `bigint out of range`.
- `tess_int8_hash(column, prepared, rows, nulls, hashes, valid, status)`
  and `tess_int8_hash_next(column, prepared, nulls, hashes, valid,
  status)`: as the int4 hashes, with the value folded to 32 bits as
  PostgreSQL's `hashint8` folds it (the low half xor the high half, the
  high half inverted for a negative value) before `murmurhash32`. An int8
  inside the int4 range hashes exactly like the int4 of the same value, so
  the two families chain in any order and an int4 key and an int8 key of
  one join share a table; equal hashes never prove int8 keys equal.
- `tess_int4_to_int8(column, prepared, rows, values, non_nulls, status)`:
  the selected int4 values widened into int8 Datums, with the output
  contract of the arithmetic; the cast `int8(int4)` as a kernel, so that a
  chain over an int4 column continues with the int8 operators.
- `tess_int8_to_int4(column, prepared, rows, values, non_nulls, status)`:
  the selected int8 values narrowed into int32 values, with the same
  output contract; a selected value outside the int4 range fails with
  22003, "integer out of range", as the cast `int4(bigint)` does. It
  serves an explicit cast only, row by row: the functions over an int4 and
  an int8 widen instead ([function.md](function.md)).

## Sets

`x IN (…)` and `x NOT IN (…)` of integer constants
(`tessera_kernels::set`, declared in `kernels.h`) take the constants once
a plan, in the value's width, sorted in increasing order without repeats:

- `tess_int4_in_set(column, prepared, keys, nkeys, rows, found, present,
  status)`: of the selected rows, `found` gets those whose int4 value one
  of the int32 keys equals and `present` those whose value is not NULL;
  every row bit of both is written. int2, date and bool words are read as
  their int4; a key beyond the int4 range equals no value and is left out
  by the caller.
- `tess_int8_in_set(…)`: the same over int8 values with int64 keys, for
  int8, timestamp and timestamptz words.

The caller makes IN and NOT IN, and their NULLs, of the two masks. A full
word of at least a dozen selected rows is compared with vector code while
the keys are few: its 64 lanes stay in registers while each key goes by,
up to 64 int4 or 16 int8 keys (four int4 lanes to a vector against two
int8 ones; beyond these bounds halving costs a row less). Every other word
is read at its selected rows: up to 16 keys compared all, without a branch
a key; more searched by halving, the comparison picking the half as data,
so that a row costs the same whatever its value. Over 2 M rows an int4 `IN`
of 2 to 100 constants costs 1.02 to 1.35 times a single comparison, and
runs 0.20 to 0.28 of the core's time.

## Decimals

`tessera/decimal.h` declares the kernels of decimals: numeric values of at
most 18 digits as an int64 at their display scale
(`tessera_kernels::decimal`). They read a numeric Datum in place, from a
varlena of either header (a compressed or external one is not read), and
refuse what is not a decimal: NaN, an infinity, a display scale past 18, a
value of more than 18 digits; such a row is left to the caller, which
takes it by the core's function. They read the selected rows alone, so a
selected row's NULL flag and non-NULL value must be initialized, and a
column's decimal side (`TessDatumColumn.decimal_rows`, see
[batch.md](batch.md)) is read as decimals. An argument (`TessDecimalArg`)
is a column or, without one, a scalar numeric.

- `tess_decimal_filter(op, left, right, rows, rest, status)`: narrow `rows`
  to the rows where the comparison (`TessCompareOp`) of two decimals holds,
  in `numeric_cmp`'s exact order; a row with a NULL leaves, a row whose
  arguments are not both decimals leaves too and is set in `rest`.
- `tess_decimal_compute(op, left, right, rows, scale, values, scales,
  non_nulls, decimals, rest, status)`: `+`, `-`, `*` (`right` of them),
  negation and `abs`: each row of `non_nulls` not in `rest` gets its exact
  result's value and scale (the larger for `+` and `-`, the sum for `*`, up
  to 36), a result at `scale` also its bit in `decimals`; an argument that
  is not a decimal, or a result past 18 digits, is left in `rest`.
- `tess_decimal_to_int4` and `tess_decimal_to_int8(arg, rows, values,
  non_nulls, rest, status)`: the casts, rounding half away from zero; a
  value outside the int4 range is left in `rest`, where the core raises
  its error.
- `tess_decimal_read(column, rows, scale, values, scales, decimals,
  status)`: a column's decimals into `values` (a Datum holding the int64,
  the other rows' Datums copied) with their bits: of one scale (`*scale`,
  the first decimal's when -1) when `scales` is NULL, else every one with
  its scale in `scales`.
- `tess_decimal_sum(column, rows, sum, rest, status)`: the rows' decimals
  added to a running sum (`TessDecimalSum`: an int128 at the largest scale
  met, below 10^36, and a count); a row that is not a decimal, or that
  would take the sum to the bound, is left in `rest`.
- `tess_decimal_write(values, scales, scale, rows, space, len, used,
  status)`: each row's decimal replaced by the pointer to its numeric,
  written as `make_result` writes it into `space` at MAXALIGN'd offsets;
  `tess_decimal_read_datum` and `tess_decimal_write_datum` read and write
  one numeric.

The functions of numeric (`kernels/numeric.c`) call them for a batch at a
time and take the rows they leave by the core's functions; the batch of a
heap scan reads a shared column's decimals, the aggregate node sums and
reads its argument's, and the expression compiler reads a constant's scale
and writes the numerics of a chain's decimals, through `TessKernelOps`.
A call costs about 10 ns besides its rows, so the callers make one a batch.

## Calendar

`tessera/calendar.h` declares the kernels of dates and timestamps
(`tessera_kernels::calendar`) the date functions of `kernels/date.c` call,
a batch a call: a date is an int4 Datum, a timestamp an int8 Datum, an
interval a pointer to the core's `Interval`, and an argument
(`TessCalendarArg`) a column or a scalar. The Julian day routines are the
core's arithmetic (`date2j`, `j2date`, `j2day`, `date2isoweek`,
`date2isoyear`), tested against another derivation on every day of the
first three million and the last two million and a million between. A
call fails where the core raises, with `TESS_ERROR_DATA_EXCEPTION`,
SQLSTATE 22008 and the core's message, at the first row the core would.

- `tess_date_arith(op, left, right, rows, values, non_nulls, status)`: a
  date plus or minus days, the days between dates.
- `tess_date_to_timestamp` and `tess_timestamp_to_date(arg, rows, values,
  non_nulls, status)`: the casts.
- `tess_timestamp_trunc(unit, arg, rows, values, non_nulls, status)`:
  `date_trunc` of a timestamp; `tess_timestamp_trunc_local(unit, locals,
  rows, values, days, rest, status)` the same over local times, a unit of a
  day and above giving the Julian day of the period's first day for the
  caller to find its midnight in its zone.
- `tess_timestamp_add_interval` and `tess_date_add_interval(minus, left,
  right, rows, values, non_nulls, status)`: an interval added as
  `timestamp_pl_interval` adds it, months clamped to the month's last day,
  then days, then microseconds.
- `tess_date_extract` and `tess_timestamp_extract(field, arg, rows, values,
  scales, non_nulls, rest, status)`: the fields of `extract`, each a value
  at a scale (6 for seconds, 3 for milliseconds, else 0), an infinite value
  left in `rest`.

The session's time zone stays in C: the local times of a timestamptz, the
midnights of local days (both kept in caches of the process), the unit
names and the rows the kernels leave.

## Text

`tessera/text.h` declares the kernels of strings (`tessera_kernels::text`)
the text functions of `kernels/text.c` call, a batch a call. A string is
read in place behind its Datum, a varlena of either header; a compressed or
external one the kernels leave in `rest`, and the caller detoasts it for
its row and calls again with a column of that one row, so no batch holds
more than one detoasted value. Characters count as the database encoding
counts them (`TessTextChars`: a byte each, or UTF-8 by its lead bytes).

- `tess_text_compare(equal, bpchar, left, right, rows, rest, status)`:
  equality or inequality of text, or of bpchar without trailing spaces.
- `tess_text_starts_with(column, prefix, len, rows, rest, status)` and
  `tess_text_like(column, pattern, len, negate, rows, rest, simple,
  status)`: a prefix, and a LIKE pattern of literals and `%` matched by its
  pieces (the search for a piece's first byte by the `memchr` crate, which
  matched the C library's `memchr` where the standard library's search was
  9 % slower on `LIKE '%12%'`); `*simple` false for a pattern with `_` or an
  escape, which the core's function matches.
- `tess_text_lengths(length, chars, column, rows, values, non_nulls, rest,
  status)`: characters, a bpchar's characters, or bytes.
- `tess_text_pieces(piece, first, second, has_second, chars, column, rows,
  starts, lengths, non_nulls, rest, status)`: the bounds of `substring`,
  `left`, `right` and the trims, which the caller copies; a substring of
  negative length fails with 22011 at the first row without NULL.
