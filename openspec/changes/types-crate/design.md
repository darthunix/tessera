## Context

Tessera's batches carry PostgreSQL's `Datum` values: an integer, a date
or a time in the word itself, a string or bytes through a pointer to a
value with a header of its length. A Parquet file, read by Apache
Arrow's Rust implementation, comes out as Arrow arrays: a buffer of the
column's values and a bitmap of its `NULL` values, strings as a buffer
of bytes and a buffer of offsets. The entry `parquet-fdw` reads such
files into batches, and a later `COPY` would write batches into them.

The two sides disagree in small ways that each lose data when missed:
the epochs of dates and times (1970 against 2000), their units
(seconds, milliseconds, microseconds, nanoseconds against
microseconds), their ranges, which overlap but neither holds the other
(PostgreSQL's last thirty years of `timestamp` have no microseconds
since 1970 in 64 bits), the infinities PostgreSQL has and Arrow has
not, the rule that `text` is valid in the server's encoding and holds
no zero byte, the length of a `varchar` in characters, the precision of
a `timestamp(p)`, the gigabyte past which PostgreSQL holds no value,
and the integers that a file keeps without a mark of what they count.

The maintainer decided on 2026-11-21 that this boundary is a crate of
its own, `datum-arrow`, free of Tessera, built before the Parquet
wrapper, that speaks Arrow's types, and that checks a column's
conversion once, before any value is converted.

## Goals / Non-Goals

**Goals:**

- One place that says which PostgreSQL type an Arrow column may become,
  and the reverse, so that a reader and a writer agree.
- A plan checked once a column, which names every way a value may still
  fail, so that the work per value is the plan's checks and nothing
  more, after one pass over the call's buffers that checks their shape.
- Conversions of values that give PostgreSQL's binary representations,
  testable without a server.
- Comparisons of stored values with a PostgreSQL constant, in the
  file's unit, without converting any value.

**Non-Goals:**

- Reading or writing files, or Arrow's arrays: the crate converts
  values a caller hands it, from arrays a caller owns.
- The headers of PostgreSQL's values of variable length, and the memory
  they live in: the caller makes them from the bytes the crate checks.
  A value passed by value the crate gives as the word of a `Datum`, so
  that a column of a batch is filled in one pass.
- `numeric`, `interval`, `uuid`, times of day, nested types, unsigned
  64-bit integers and nanoseconds: later, each an entry of its own.

## The whole

```
   Parquet file ── parquet crate ──► Arrow arrays (the caller's)
                                          │
   datum-arrow:
     ReadPlan::new(arrow type, pg type, options) ─► refused, or a plan
     plan.failures() ─► the errors a caller may meet
     plan.read(slots asked, bitmaps) ─► PG representations
     plan.test(comparison, constant) ─► a test of stored values
                                          │
   the caller: text headers, the batch ◄┘

   batches ─► the caller: representations ─► datum-arrow:
     WritePlan::new(pg type, encoding) ─► Arrow type
     plan.write(slots asked) ─► Arrow values ─► parquet crate
```

A plan holds the pair, the options and the failures it allows. A
conversion of values takes a plan, a column's slots, its bitmaps of
`NULL` values (a dictionary's too) and the slots the caller asks for,
checks the buffers' shape first (a write checks the size of its values
between the shape of its input and that of its output), and fills the
representations of the slots asked, or the reverse; it skips a `NULL`
slot, and stops at the first slot asked that fails, with its slot and
its error.

## Decisions

### A crate free of Tessera and of PostgreSQL

The crate knows PostgreSQL's types by their object identifiers, fixed
for the built-in types, and their binary representations, fixed in
PostgreSQL's on-disk format; it links no PostgreSQL library and calls
none. So it builds, tests and packs alone, and serves any extension or
tool on either side. Refused: the conversions in the C module of the
wrapper, where a writer later would have to repeat them, and where
nothing tests them without a server.

### Arrow's types, not Parquet's schema

Apache Arrow's Rust implementation already turns a Parquet column into
an Arrow array and back; its rules of physical types and marks are its
own. The crate speaks Arrow's `DataType`, through the small
`arrow-schema` crate, so that a reader and a writer over Parquet share
one boundary, and so would one over another format that Arrow reads.
What Arrow loses of a file, an integer that counts days or seconds
without a mark, comes back as the plan's unit, an option the caller
gives. Refused: the mapping from Parquet's physical types and marks,
which a writer through Arrow would have to repeat on its side.

### A plan checked once

A plan is made from the two types and the options and is refused when
the pair cannot be converted; an accepted plan lists its failures, and
only those a value of its Arrow type can meet. A conversion then runs
only the checks its plan lists: an integer widened runs none, nor does a
day in 16 bits, which every `date` holds; a day in 32 bits checks its
range; a string checks its zero byte, checked bytes UTF-8 too, a
`varchar` its characters, and a value of variable length its size. The
kinds of failure are few and closed, and each names its SQLSTATE, so
that a caller inside a server raises the error PostgreSQL would raise in
the same case.

Where PostgreSQL's input would change a value, the crate refuses
instead: a `timestamp(p)` below microseconds rounds on input, so only
the precisions that keep a microsecond are known; a `varchar(n)` cuts
spaces past its length on input, so a string longer by spaces fails
here. A plan never changes a value; it converts it or names how it may
fail.

### Conversions on slots

Values come as Arrow's buffers hold them, with the column's bitmap of
`NULL` values, and go out into slices the caller gives: no allocation
per value, and loops a compiler can unroll. A `NULL` slot holds bytes of
no meaning and never fails, and its place in the output of a read has no
meaning; a `Boolean` comes as bits from an offset. A `Dictionary`'s
values are converted where a slot refers to them, so that a value no row
uses never fails. A string's bytes are checked in place; the caller
copies them into PostgreSQL's value with its header.

### Loops a compiler vectorizes

The conversions are elementwise arithmetic with a check of range: an
epoch shifted, a unit multiplied, an integer widened. Such a loop runs
on the vector units of AArch64 and x86-64 without intrinsics when it
has no branch per value, so the hot loops are written that way:

- the slots asked come as a mask of 64-bit words, the form of a batch's
  rows, so that a loop takes 64 slots at a time;
- an array of a type of fixed width, not a dictionary, whose indexes are
  read only for the slots asked, is converted for every slot of a run,
  asked or not, into the output's places, so that the loop is dense: a
  slot not asked that is not `NULL` gets its true representation where
  its value converts, so that a place a caller filled before keeps its
  value, and any other place a value of no meaning;
- a check folds its result over the 64 slots into one word, masked by
  the slots asked and not `NULL`, and the first failing slot is the
  first set bit of that word.

Strings and bytes are checked and given for the slots asked alone: a
string's check is a loop over its own bytes. A write packs the slots
asked, one Arrow value each, so it is not a dense loop: it pays a
compression of the slots asked, which a write of every slot, as a `COPY`
makes, skips by a mask of all ones. The machine code of each hot loop is
read on both machines, from `cargo rustc --release -p datum-arrow --lib
-- --emit asm` for AArch64 and for x86-64, before a loop is called
vectorized, and kept in the Performance item of the pull request.
Explicit intrinsics come only where that code and a measurement show the
compiler's loop falls short, each behind the scalar loop as its
reference.

Refused:

- **A crate of its own for the vector code.** The conversions have one
  owner, this crate; a crate of vector code that shifted epochs or
  checked UTF-8 would be a second. Tessera's kernels, which hold its
  own vector code, are not a dependency of a crate free of Tessera.
- **A list of slot numbers for the slots asked.** It makes every loop a
  gather, which neither machine vectorizes well.
- **`simdutf8` for the check of UTF-8 now.** The standard library's
  check is fast already; the crate is a candidate once a measurement
  shows the check costs.

### Proofs and tests

The conversions of one value are small and closed, which suits Kani, a
checker that proves a property of a Rust function for every input
rather than for a sample. Kani proves, of the scalar code of one value:

- the test of a bound against the exact comparison in 128 bits, for
  every stored value and every finite constant, for integers, days and
  microseconds; the bounds of seconds and milliseconds divide the
  constant by a million or a thousand, and a symbolic division of 64
  bits takes Kani past the ten minutes of a harness, so the property
  tests compare them with the exact count, over the edges of the
  ranges, instead;
- the answer for a range of stored values exact, for every least and
  greatest value: every value of the range passes where it says every
  value, none where it says no value, and where it says some may, the
  range holds a value that passes and one that does not; a range turned
  the wrong way gets some may;
- a day or a time read without an overflow and accepted exactly when it
  is in PostgreSQL's range; written without an overflow and accepted
  exactly when it is in both PostgreSQL's range and Arrow's; and every
  value written read back to the same representation;
- for plans of integers, of days in 16 bits, of 64-bit integers and of
  seconds, and every value of a column of one slot, a failure exactly
  where the value is out of range and only where the plan names it, the
  check of the contract's rule "A plan names every failure" on these
  plans;
- a string of up to four bytes read through a plan of checked bytes
  into `text`: refused, with `22021` and its slot, exactly where it is
  not UTF-8 or holds a zero byte, and given as its bytes otherwise; and
  the count of a `varchar`'s characters against the characters the
  standard library decodes from a valid string, for strings of up to
  four bytes. The check of UTF-8 itself is the standard library's,
  which the proof calls on both sides: it shows the plan's wiring, not
  UTF-8.

Kani's cost grows with the length of a loop it unrolls, and it does not
know most vector intrinsics, so it proves the code of one value; the
loops over a batch, and any vector code, are compared with that code by
property tests, Miri checks the code that is `unsafe`, and the mutants
of a pull request check that the tests catch a change. The crate takes
a row of the Kani job of CI, as `relhash` and `relspill` have.

### The bounds of a comparison

A reader that checks a clause such as `EventTime >= '2013-07-01'` on
the file's seconds turns the constant once into a test of the stored
integers: a bound in seconds, rounded up or down as the operator needs
(`= 12.5 s` passes no whole second), or "every value", or "no
value". The test is exact over the whole stored domain, as if every
stored value were counted in microseconds since 2000 in 128 bits, so
that a stored value outside PostgreSQL's range compares by its true
value, without an error, and only the value made of a row that passes
may fail. The infinities are above and below every stored value. An
integer column takes a constant of any integer type, as PostgreSQL's
B-tree family of integers compares `smallint` with `bigint`:
ClickBench's `AdvEngineID <> 0` compares a `smallint` with an
`integer`. The
bounds are for integers, days and times alone: a floating-point number
orders `NaN` as PostgreSQL does not, and a string by its collation.

The constant is divided by the stored unit in 64 bits: every epoch
shift is a whole number of its stored unit (946 684 800 000 000
microseconds are as many whole milliseconds and seconds), so
floor((c + shift) / unit) is floor(c / unit) plus the shift in stored
units; `Count::time` asserts it, and a proof checks it of every count.

### Decisions of the maintainer

On 2026-11-21:

- The boundary of types between PostgreSQL and Parquet is a crate of
  its own, both ways, since a `COPY` into Parquet files will need the
  write side, with each conversion checked before any value is
  converted.
- It comes in a change of its own, before `parquet-fdw`, based on
  `main`, since the crate needs nothing of `source-scan`.
- It speaks Arrow's types.
- It is free of Tessera and `publish = true`, as `relhash` and
  `relspill` are.
- Its name is `datum-arrow`: it says what the crate converts, the
  `Datum` values inside the server, unlike the crates `arrow-pg` and
  `pgpq`, which encode Arrow's values in PostgreSQL's protocol and its
  `COPY BINARY`; a first name, `pgarrow`, was too close to `arrow-pg`.
  Apache Arrow's Rust crates have a trait `Datum` of their own, a value
  or an array given to a kernel, which the crate's documentation tells
  apart.
- The code of one value is proved by Kani in CI; the loops over a batch
  are written for the compiler to vectorize, the slots asked given as a
  mask, with no crate of its own for vector code and explicit
  intrinsics only where a measurement asks for them.
- The default build of x86-64 is kept, its loops scalar but without a
  branch per value, with no choice of code at run time.
- Kani proves the bounds of integers, days and microseconds; the bounds
  of seconds and milliseconds, which Kani does not finish within the
  limit of a harness, rest on the property tests against the exact
  count.

## Was and will be

- **The boundary of types.** Was: none. Will be: `datum-arrow`.
- **The workspace.** Was: no dependency of Apache Arrow. Will be:
  `arrow-schema`, `arrow-array` for the crate's tests, then the
  `parquet` crate in `parquet-fdw`.

## What we do not do

- **`numeric` and `Decimal128`.** PostgreSQL's numeric is a sequence of
  base-10000 digits; its conversion is a change of its own, with
  `COPY`'s needs.
- **Nanoseconds now.** PostgreSQL holds microseconds, and a plan never
  drops digits: a plan of nanoseconds would name a new failure, a value
  with digits below a microsecond.
- **A C API.** The wrapper reaches the crate from Rust, through its
  reader; a C API waits for a caller in C.

## Risks / Trade-offs

- [PostgreSQL changes a type's representation] → the representations
  of the built-in types are part of its on-disk format, which a major
  release keeps; the wrapper's suite compares the crate's results with
  the server's own.
- [Arrow's crates move fast] → `arrow-schema` takes a new major
  version several times a year, and its `DataType` is in
  this crate's public API, so a release of `datum-arrow` works with one
  major of Arrow, the one the `parquet` crate of the workspace needs,
  and its API snapshot changes with each move.

## Measurement plan

None: no hot path of Tessera changes in this change. The conversions'
speed is measured in `parquet-fdw`, where they run.

## Open Questions

None that would change the spec or the tasks.
