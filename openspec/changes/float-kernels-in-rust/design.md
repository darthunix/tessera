## Context

See proposal.md, "Why", for the motivation.

A batch function gets a call: up to 64 rows a word, a selection mask
(the rows to compute), and its arguments. An argument is a column of
Datums with a NULL flag a row, or a scalar Datum, the same for every
row. A comparison narrows the selection to the rows where it is true.
Any other function writes a value for each selected row and a mask of
the rows that are not NULL.

Today `kernels/float.c` does all of this in C:

```
  consumer (a node's expression)
     |  TessFunctionCall: selection, two arguments, output buffers
     v
  kernels/float.c
     float_compare_evaluate / float_value_evaluate
       for each selected row:
         read the Datums, skip NULLs
         switch on the operation
         call the core's inline function of utils/float.h
           (float8_lt, float4_pl, rint, ...)
         on overflow: ereport(ERROR) jumps out of the loop
```

The calendar, numeric and text functions were moved to Rust before
and have this shape, which the float functions will take:

```
  consumer
     |  TessFunctionCall
     v
  kernels/date.c          registration, the call's checks, the parts
     |                    that need PostgreSQL (time zones)
     |  one call a batch, tess_date_arith(...), a status back
     v
  tessera-capi c/calendar.rs   reads the arguments behind raw pointers,
     |                         catches a panic, fills the status
     v
  tessera-kernels calendar     safe Rust: the rules and the row loops
```

Rust never calls PostgreSQL, and a PostgreSQL error never jumps across
a Rust frame: Rust returns a status, and C raises it after the call.

## Goals / Non-Goals

**Goals:**

- Every rule of the float functions in one place in safe Rust, each
  tested against a reference written another way.
- The same results, errors, SQLSTATEs, messages and erring row as
  today, for every one of the 58 functions in every shape: two
  columns, a column and a scalar on either side.
- No query slower than with the C version (see "Measurement plan").
- One reader of a column-or-scalar argument and one row loop, shared
  by the calendar and the floats, instead of a copy for each.

**Non-Goals:**

- Vector code for floats. The loops stay scalar; a measured need comes
  first.
- The float transitions of the generic aggregates
  (`fast_float_advance` in `nodes/agg_generic.c`). They are a later
  step over the same rules.
- Changing which functions are batch functions, or any plan.

## Decisions

### 1. The rules are written in safe Rust from the core's source

- **Was:** C calls the inline functions of `utils/float.h` (`float8_eq`
  …, `float4_pl` …, `float8_div`) and the checks of `dtoi4`, `ftoi4`,
  `dtof` and their siblings, copied in `float_to_integer` and
  `float_cast`.
- **Will be:** a module `tessera_kernels::float` with one small function
  for each rule:
  - comparison in the core's order: NaN equals NaN and is greater than
    every number; -0 equals 0;
  - arithmetic of f32 and of f64: an infinite result from finite
    operands is "value out of range: overflow"; a zero product of two
    non-zero operands, or a zero quotient of a non-zero dividend by a
    finite divisor, is "value out of range: underflow"; a zero divisor
    of a dividend that is not NaN is "division by zero", checked before
    the others;
  - mixed float4 and float8 operands widen the float4, which is exact;
  - a cast to an integer rounds half to even (`round_ties_even`, which
    is C's `rint` in its default mode), then checks the range the core
    checks, `x >= MIN && x < -MIN` in the source's width, NaN out of
    range;
  - a cast from float8 to float4 checks overflow and underflow as
    `dtof` does; integer to float rounds to nearest, as C does.
- **Why:** these rules are a few lines each and stable in the core.
  Written out, a test can reach each one, and a mutant of each one has
  a judge.
- **Refused:** calling C's `rint` or the core's functions from Rust.
  Rust never calls C or PostgreSQL in Tessera; the standard library has
  the same rounding.

### 2. Three entry points, by what a call writes

- **Was:** two C functions, one for comparisons and one for every
  function that writes a value.
- **Will be:** `include/tessera/float.h` declares:
  - `tess_float_compare(op, left type, left, right type, right,
    rows, status)`: keeps in `rows` the selected rows where the
    comparison is true, as the integer filters do;
  - `tess_float_arith(op, left type, left, right type, right, rows,
    values, non_nulls, status)`: `+ - * /`, unary minus and `abs`
    (a unary call passes its argument once), Datum results;
  - `tess_float_cast(from, to, arg, rows, values, non_nulls,
    status)`: `values` holds int32 when the result is int4 or int2,
    Datums otherwise, as the function's result format says today.

  The codes of the operations and of the types move from `float.c`
  into the header as `TessFloatOp` and `TessFloatType`. An argument is
  a `TessFloatArg`: a column, or a scalar Datum when the column is
  NULL, as `TessCalendarArg` is.
- **Why:** the three write different things, so each gets typed
  outputs. Given the operation and the types once a call, Rust picks
  one loop compiled for that operation, those types and that shape, so
  no row pays for a `switch`.
- **Refused:** an entry point for each SQL function (58 symbols for
  one rule each), and one entry point for all (a comparison writes a
  mask, the others values).

### 3. One reader of an argument and one row loop

- **Was:** `c/calendar.rs` has a private reader of a column-or-scalar
  argument (`Values`, `column`, `input`), and
  `tessera_kernels::calendar` a private trait `Source<T>` and loop
  `map_rows`.
- **Will be:** a mechanical commit moves the reader into `c/source.rs`
  and `Source<T>` with the loop into a module of `tessera-kernels` that
  both families use; the loop becomes generic over the error type.
  `tessera-capi` reads a float8 Datum as `f64::from_bits` and a float4
  Datum as `f32::from_bits` of its low 32 bits, through the reader's
  trait of a Datum's value, as it reads an interval; the public
  `FromDatum` stays the integers'. A float4 result is written as
  `Float4GetDatum` writes it: its bits as an int32, sign-extended.
- **Why:** one owner of how an argument is read and of the order in
  which rows are computed: the rows go in ascending order and the first
  error ends the call, so an error comes from the row the core would
  stop at.
- **Refused:** a third copy of the reader for floats.

### 4. Errors are values with the core's SQLSTATE and text

- **Was:** `ereport` inside the C loop, through `float_overflow_error`,
  `float_underflow_error`, `float_zero_divide_error` and the "integer
  out of range" of the casts.
- **Will be:** an enum `FloatError` (overflow, underflow, division by
  zero, integer, smallint and bigint out of range), each with its
  SQLSTATE (22003, or 22012 for division) and the core's message, as
  `CalendarError` does. `error.rs` classifies it as an error of the
  data; the status carries its SQLSTATE and message, and
  `tess_status_report` raises them as the core does.
- **Why:** the expected outputs of the suites stay byte for byte, and
  no error jumps out of a loop.

### 5. C keeps the registration and the call's checks

- **Will be:** `kernels/float.c` keeps the table of the 58 functions
  with their OIDs and types, the check of a call's shape, and two thin
  functions that build the arguments and call Rust once a batch. It
  holds no arithmetic and calls no `ereport`.

## What we do not do

- **Vector code.** A float column is read as Datums a row at a time;
  NaN and -0 make a vector comparison more than one instruction. No
  measurement asks for it yet.
- **A spec of the kernels.** The kernels have no capability folder;
  writing one for floats alone would split their description. The
  header and `docs/function.md` describe the entry points.
- **The aggregates' float transitions.** They stay in
  `nodes/agg_generic.c`; moving them is a later step over the rules of
  decision 1, keeping the core's order of additions.

## Risks / Trade-offs

- [A rule copied wrong changes a result] → the Rust tests compare every
  rule with a reference written another way on the edge values of each
  type, and the SQL suite `types` compares Tessera on and off.
- [Another float environment: rounding mode, flush of subnormals] →
  PostgreSQL itself runs in the default environment, round to nearest
  without flushing, which Rust's operations assume too. Subnormal
  operands and results are among the edge values of the tests.
- [The shared loop moves the calendar's code] → that commit only moves
  lines (`cargo ab moved`), and the disassembly of the kernels module
  shows the calendar's loops unchanged.
- [A slower query] → the measurement plan below; a loop compiled for
  each operation and type has fewer branches a row than the C one.

## Tests

- `tessera-kernels`: unit and property tests of each rule against a
  reference written another way, over all pairs of edge values (±0,
  ±infinity, NaN, the largest and smallest normal and subnormal values,
  the edges of each integer range, ties of rounding) and random bit
  patterns; the batch functions over NULLs, sparse and dense selections
  and more than one word, with the first error at a later row.
- Kani: for every float, a cast to an integer that is accepted gives
  the rounded value, and one that is refused is out of range. If Kani
  cannot finish within the job's limit, an exhaustive test around each
  bound takes its place, and the pull request says so.
- `tessera-capi`: a test of every entry point through its raw C
  structs, every operation, type pair and shape, with NULLs and errors;
  the Miri job runs it.
- SQL: the suite `types` gives the same output, with rows added for
  what it lacks: a subnormal, `NaN / 0`, `0 / 0`, infinity times zero,
  the float4 overflow of a product, the edges of the casts.
- Mutants of the changed lines: none survives, or each is listed as
  equivalent with its reason.

## Measurement plan

- **What:** the time of queries whose float functions Tessera
  computes, against the C version.
- **Cases:** three cases added to the family `win` of `bench/pg` over
  `bench_narrow`, in a first commit: a filter
  (`c1::float8 * 0.5 > constant`), arithmetic and a cast to an integer
  under a sum (`sum((c2::float8 / 3)::int8)`), and float4 against
  float8 (`c3::float4 * 2::float4 < c4::float8`).
- **Tool:** `cargo ab measure --base <the commit with the cases>
  --family win --cases '^float_'`, release builds, on an idle machine
  that the person asks for, at most five minutes.
- **Threshold:** no case flagged by the tool, that is, no case slower
  beyond the drift of the core's own time between the sides.
- **Machine code:** `tessera-bench --disasm --module kernels` on the
  commit that moves the shared loop: the calendar's loops the same.

## Migration Plan

None: the extension is built and installed as one; no format or
setting changes.
