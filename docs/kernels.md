# Kernels from C

The Rust kernels of `tessera-kernels` reach C through `tessera-capi`, a
static library (`target/<profile>/libtessera_capi.a`) declared in
`include/tessera/kernels.h`. An entry point reads a column as the batch
contract's `TessDatumColumn` (Datum values and NULL flags), reads or narrows
`TessRowMask` bitmaps, and returns a `TessStatusCode` that it also stores in
the caller's `TessStatus`. It never raises `ERROR`: PostgreSQL's error
mechanism jumps over stack frames, and Rust frames must unwind, so the
caller reports the status after the call returns.

That holds for these entry points, not for the batch functions of the
registry (`TessFunction.evaluate`, `tessera/function.h`) that the
`tessera_kernels` module builds on them in C. Those raise the errors the
PostgreSQL function they implement raises, as `function.h` promises: the
Rust kernels' errors come back in the status and the wrapper reports them,
and for the rows the kernels leave (a value of a type or size they do not
take) the wrapper calls the core's function, which raises `ERROR` itself.
A consumer treats `evaluate` as a call that may raise. Its scratch, past
a size on its stack that a batch of 64 rows never reaches, is allocated in
the current memory context and goes with it.

The same library exports the hash table of joins and grouping, declared
in `include/tessera/table.h` and described in the capability
[hash-table](../openspec/specs/hash-table/design.md): its calls take a
region of memory the caller owns and follow the same status rules.

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
header the caller was compiled with, and `tess_kernels_probe()` returns the
sizes and offsets the library was built with. `tessera/kernels_layout.h`
compares them, with the table, sort and spill probes, against the headers
in `tess_kernels_layout_matches()`; the `tessera_kernels` module runs it
when it loads and refuses a library built from another source tree with
"incompatible Tessera kernels library layout".

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
which share the trait `SqlError`, and an error of `relspill` of the kind
`relspill::Kind::Damaged`) from every other error, which is a misuse of
the call, the caller's bug, and comes back as `TESS_ERROR_INVALID_ARGUMENT`
with `XX000`.

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
- `tess_int2_arith_scalar`, `tess_int2_arith_scalar_left` and
  `tess_int2_arith_columns`, with the arguments of the int4 ones: the
  smallint arithmetic, over int4 lanes that hold smallints, into a dense
  int4 result within the smallint range (`tessera_kernels::smallint`). A
  result past it reports `22003` "smallint out of range"; each row's
  operation and its check are one step, and the vector code of a whole
  word flags a lane past the range as it flags an overflow, so the call
  fails with the error of the first row that fails, a range error or a
  zero divisor, as the core does. The operands must be smallints, but
  for `x + 0`, the cast `int2(int4)`, whose `x` may be any int4.
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
  int8, timestamp, timestamptz, time and money words.

The caller makes IN and NOT IN, and their NULLs, of the two masks. A full
word of at least a dozen selected rows is compared with vector code while
the keys are few: a pass of 32 int4 or 16 int8 lanes stays in registers
while each key goes by, up to 64 int4 or 16 int8 keys. A vector holds
four int4 lanes or two int8 ones on NEON, and eight or four on AVX2; a
pass is eight vectors on NEON and four on AVX2, which has half as many
registers. Beyond these bounds halving costs a row less. Every other word
is read at its selected rows: up to 16 keys compared all, without a branch
a key; more searched by halving. A step of halving picks the half as data
on AArch64, so that a row costs the same whatever its value, and is a
branch on x86-64, where a choice made as data cost more cycles in the
benchmark, whose values the predictor learns. Over 2 M rows an int4 `IN`
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
  met, below 10^36, and a count), an int4 or int8 column's integers of
  any size among them at scale 0; a row that is neither, or that would
  take the sum to the bound, is left in `rest`.
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

The sums of integers take every value at scale 0, one of 19 digits too.
An int4 or int8 column hands each word's values over in bulk, which add
as an `i128`; a value of 19 digits taken alone, as after a word refused
at the bound or for a group past a word's 16, is a term of its own
kind, `Term::Integer`, so that a grouping whose keys follow one another
does not send its rows to `numeric_add`, while a shorter one stays a
`Decimal` at scale 0. No `Decimal` holds 19 digits: the rescaling, the
comparisons and the writing of numeric rest on 18. Every integer as the
new term was refused: each place that takes a decimal of scale 0 would
need its arm on the path of 18 digits too. The
bound stays 10^36 and the state is the decimals', where the core keeps
a state of its own for bigint: integers reach the bound only after some
10^17 rows of the largest values, and a second range would give the
partial format two meanings. A magnitude is compared with the bound by
`unsigned_abs`, since a bulk sum rescaled to scale 18 can land exactly
on `i128::MIN`. A sum counts its rows within an int64's most
(`SUM_COUNT_MOST`), as C keeps the count: a state past it is refused on
the way in, a term or a sum that would count past it goes to the rest,
and the aggregate node adds its own counts by one function that fails
with 22003 "bigint out of range". Proofs by Kani
(`crates/tessera-kernels/src/decimal/proofs.rs`) show the arithmetic of
the integer terms for every state the entry points let in; property
tests compare both loops with a model that adds row by row, and start
from states near the bound, which no query of integers reaches.

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
- `tess_date_timestamp_filter(column, prepared, rows, op, timestamp,
  status)`: the selected dates where `date op timestamp`, as
  `date_cmp_timestamp` orders them: the timestamp becomes once an operator
  and a bound in days (`calendar::date_bound`), and the int4 filter
  narrows `rows`. A Kani proof checks the bound against the core's
  comparison for every date, day, time of day and operator.
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
the text functions of `kernels/text.c` call, a batch a call. A string
argument is a `TessTextArg`: a column of `Datum` values, a column in the
bytes form of `tessera/batch.h`, or a scalar. A string is read in place
behind its Datum, a varlena of either header, or from the bytes form,
where every string is in place; a compressed or external value the
kernels leave in `rest`, and the caller detoasts it for its row and calls
again with a column of that one row, so no batch holds more than one
detoasted value. Each entry point has a loop for each form of each
argument, and the logic of a function is the same for all of them.
Characters count as the database encoding counts them (`TessTextChars`: a
byte each, or UTF-8 by its lead bytes).

The text functions read their string columns in the bytes form where the
expression gives it (`TESS_FUNCTION_BYTES`, [function.md](function.md)).
A row of the form they hand to the core's function, for a pattern with
`_`, an escape or more pieces than the matcher holds, or for the
characters of a multibyte encoding other than UTF-8, gets a value made
from its bytes in the call's context, a header and the bytes, which
passed their column's rule; it is freed after the row, as a detoasted
copy is. A Datum goes to the core's function as it is, which detoasts as
much of it as it needs. A row whose other argument is compressed or
stored apart is compared again by the kernels over that one row, the
value detoasted and the other side's made from its bytes.

- `tess_text_compare(equal, bpchar, left, right, rows, rest, status)`:
  equality or inequality of text, or of bpchar without trailing spaces;
  a bpchar scalar is trimmed once a call, and each value compared by
  its first bytes, its rest looked at for spaces only when they match
  (TPC-H Q19 and Q12 take 14 % to 19 % less, see
  [2026-11-02-bpchar-constant-compare](benchmarks/2026-11-02-bpchar-constant-compare/README.md)).
- `tess_text_starts_with(column, prefix, len, rows, rest, status)` and
  `tess_text_like(column, pattern, len, negate, rows, rest, simple,
  status)`: a prefix, and a LIKE pattern of literals and `%` matched by its
  pieces; `*simple` false for a pattern with `_` or an escape, which the
  core's function matches. The pieces at the start and at the end compare
  at one place. Each of the first four non-empty inner pieces is found
  by a searcher of `memchr::memmem` made once a call: over a string long
  enough for its vectors, 17 bytes on AArch64, it looks for a pair of
  the piece's rare bytes and compares the piece only where both are
  found; over a shorter string it takes Rabin-Karp, and a piece of one
  byte `memchr`. The searchers are boxed and made only for a pattern that
  has an inner piece, so that a prefix, a suffix or an exact pattern
  costs no more than without them. A piece past the fourth is found by
  `memchr` of its first byte, which matched the C library's
  `memchr` where the standard library's search was 9 % slower on `LIKE
  '%12%'`. Against that search for every piece, TPC-H query 13's `NOT
  LIKE '%special%requests%'` took 0.42 of the cycles and query 13 itself
  0.69 of its time, ClickBench's Q21, `URL LIKE '%google%'`, 0.70 of its
  cycles; strings of 8 to 16 bytes, by Rabin-Karp, take 25 % fewer cycles
  though 12 % more instructions, a searcher made once a call. The two
  other ways counted,
  `memmem::find` a row, which makes a searcher for each string past 64
  bytes, and `memchr` of a piece's rarest byte, lost on most cases or
  gained on one (see
  [2026-11-28-like-substring-search](benchmarks/2026-11-28-like-substring-search/README.md)).
- `tess_text_lengths(length, chars, column, rows, values, non_nulls, rest,
  status)`: characters, a bpchar's characters, or bytes.
- `tess_text_pieces(piece, first, second, has_second, chars, column, rows,
  starts, lengths, non_nulls, rest, status)`: the bounds of `substring`,
  `left`, `right` and the trims, which the caller copies; a substring of
  negative length fails with 22011 at the first row without NULL.
- `tess_text_sort_words(column, rows, words, non_nulls, rest, status)`:
  a string's sort word, its first eight bytes as a big-endian integer,
  padded with zero bytes, its top bit flipped, so that words compare as
  signed integers as the strings' starts do as unsigned bytes; equal
  words order by the bytes after the eighth and then by length. A proof
  by Kani shows that order is the strings' unsigned bytes', for strings
  of up to 16 bytes (`text/proofs.rs`).

## Proofs by Kani

Kani, a model checker for Rust, proves some kernels and their checks
for every input, not for the inputs a test samples. The Kani job of CI
runs each crate's proofs on a runner of its own. A harness that fails,
or that passes its time limit, fails the job, and so does a cover that
no input reaches: a proof around a branch no input reaches holds for
no value at all. Each proof's module says what it proves and its
bounds.

- The two walks over a selection word, which unsafe readers trust to
  read only rows in range, selected and prepared (`tessera-core`'s
  `reader/proofs.rs`, `tessera-capi`'s `column/proofs.rs`): a word is
  refused exactly when it calls for it, before any read; the rows read
  are the selected, prepared ones, each once, in order; nothing is read
  after a refusal or a failed fold step. The walk of `tessera-capi` is
  proven over one word of any bits, either mask absent; and over two
  words whose first is empty or full, its rows prepared and no mask of
  NULLs: 70 rows with the six rows of the second word any, or 128 with
  the second word wholly selected. Two words of any bits took Kani more
  than half an hour.
- The sort item (`tessera-kernels`' `sort/proofs.rs`): two rows' items
  compare as the rows do by their keys, NULL placed and a descending
  key reversed, then by reference, in six shapes of keys that reach
  each branch of the packing of bits into words; not every layout of
  up to 16 keys.
- The prepared divisors of int4 and int8 by a power of two
  (`int32/divisor/proofs.rs`, `int64/divisor/proofs.rs`): the quotient
  and the remainder of every dividend are the truncating ones. The int4
  proof is of the scalar formula, the specification the vector code
  (NEON, AVX2) is held to by differential tests; the int8 one is of the
  loop that divides a word's lanes on AArch64.
- The bound of a date against a timestamp (`calendar/proofs.rs`) and
  the casts of a float to an integer (`float/proofs.rs`).

Code a proof reaches is kept within Kani's reach. Kani 0.68 cannot
compile a path that builds an `anyhow` error, so the checks a proof
reaches live in a private helper that takes a function making the
error of a refusal from its reason: the code passes one that builds
the `anyhow` error in place, as `ensure!` and `bail!` did, so that its
machine code stays as it was, and a proof passes one that keeps the
reason. The helper also takes the reader, and never calls it before
its checks. A reason as a `&'static str` kept its pointer and length in
two registers across the readers' loops, and a builder out of line
merged the checks into one computed branch; neither was kept.

Left to the tests:

- the prepared divisors by a multiplier, of int4 and int8, and the
  divisions of `Decimal::round` and of the split of a timestamp: Kani
  does not finish a division or a multiplication of two unknowns, and a
  proof of the lines after such a division would restate them;
- around the sort item: the extraction of a batch's keys into slots and
  NULL bits before it is written, the comparison of an item of two
  words as one 128-bit key, and `run_words`;
- around the walks: a caller that passes another word's bits or values
  shorter than its rows, and a walk of `WordValues` that mixes `next`
  and `fold`;
- the vector code, which Kani does not model.
