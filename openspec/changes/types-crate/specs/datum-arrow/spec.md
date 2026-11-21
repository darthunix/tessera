# Spec Delta

## Purpose

`datum-arrow` is the boundary between PostgreSQL's types and the types
of Apache Arrow, a form of columns in memory into which Apache Arrow's
implementations read files such as Parquet and from which they write
them. A **read plan** says how the values of an Arrow column become
values of a PostgreSQL type, and a **write plan** how the values of a
PostgreSQL column become an Arrow column. A plan is made once for a
column, before any value is converted: it refuses a pair of types it
cannot convert, and it says every way a value may still fail, so that
the conversion runs per value only the checks the plan names and the
caller handles only those errors, besides the one error of a call of a
broken shape. The conversions of values work on PostgreSQL's
**representations**, the forms a value has inside the server, in the
machine's byte order, as a `Datum` or a value's bytes hold it, not the
format of `COPY BINARY`: a `boolean` as one byte of 0 or 1, a `date` as
days since 2000-01-01 in 32 bits, a `timestamp` as microseconds since
2000-01-01 00:00:00 in 64 bits, a `text` as its bytes without
PostgreSQL's header of length. A value of a type the server passes by
value is given as the 64-bit word of a `Datum`, as the server's own
functions make it: a `boolean` as 0 or 1, a `smallint`, an `integer`, a
`date` and the 32 bits of a `real` widened with their sign, the others
as their 64 bits. They need no PostgreSQL to run. A **slot** is the
place of one value in an Arrow array. An array may have a **bitmap**, a
bit a slot read from a bit offset, set where the slot holds a value and
clear where it is `NULL`; an array without one holds no `NULL`; the
bytes of a `NULL` slot have no meaning. Strings and bytes of variable
length are kept as one buffer of bytes and a buffer of **offsets**,
where each slot's bytes begin and end. A **dictionary** array keeps a
list of values once and an index into it a slot. The slots a call asks
for are given as a **mask**, 64-bit words of a bit a slot, from the
lowest bit of the first word. ClickBench, a benchmark of analytic
queries, is the first user of the crate, through a reader of its Parquet
files. The requirements go from the crate as a whole and its errors to
the types it knows, the two plans, the values read and written, the
round trip and the bounds of a comparison in a file's own unit;
[design.md](design.md) explains the whole and the reasons.

## ADDED Requirements

### Requirement: A crate of its own
The boundary SHALL be the Rust crate `datum-arrow`, which depends on no
crate of Tessera and on no code of PostgreSQL, and of Apache Arrow on
`arrow-schema` alone outside its tests, and SHALL build, pass its tests
and pack alone. Its documentation SHALL be complete by itself: every
public item SHALL say what it is for, what it takes and gives, the
errors it returns, and the rules and limits of the API.

#### Scenario: The crate packs alone
- **WHEN** the crate is packed, as for publishing
- **THEN** its library builds from the packed files and the crates it
  names, of which none is of Tessera's workspace and only `arrow-schema`
  of Apache Arrow's, `arrow-array` named for its tests alone
- **Verified by:** pending

#### Scenario: Every public item documented
- **WHEN** the crate is built with documentation required of every
  public item
- **THEN** it builds without a warning
- **Verified by:** pending

### Requirement: Errors, never panics
No call SHALL panic, whatever its input. A refusal of a plan or a
failure of a value SHALL be an error of one of these kinds, each with
its SQLSTATE: a type, a plan or a server the crate does not support,
`0A000`; a pair of types it does not convert, `42804`; an option that
does not apply, `22023`; a value outside the range of its target type,
`22008`; bytes that are not UTF-8, or a zero byte, in a text, `22021`; a
string longer than a `varchar`'s length, `22001`; a value read larger
than the 1 073 741 819 bytes that leave room in PostgreSQL's largest
value for the header of length a caller adds, or values written in one
call past the 2 147 483 647 bytes that the offsets of `Utf8` and
`Binary` hold, `54000`, as PostgreSQL raises when a string passes its
limit. Where a value meets several, its size SHALL be checked first,
then its UTF-8 and its zero byte, then its length, as PostgreSQL checks
a string's encoding before a `varchar`'s length. That the bytes of
`Utf8` and `LargeUtf8` are UTF-8, as Arrow's format requires, SHALL be
the caller's to keep, unchecked: a caller that cannot vouch for it
reads the column's bytes as `Binary`, checked; a `Utf8` value that is
not UTF-8 gives a result the crate does not promise, but never a panic.
A failure of a value SHALL give the slot it came from.

#### Scenario: No panic on any input
- **WHEN** plans are made from every pair of known types with every
  option and every type modifier, and calls are made over slots of
  every bit pattern
- **THEN** every call returns its result or an error of the listed
  kinds, and none panics
- **Verified by:** pending

### Requirement: Calls of a broken shape
A call whose buffers break the shape of its arrays or of the call, such
as a bitmap, a buffer of values or of offsets, or an output, shorter
than the slots asked; a slot asked past the column; offsets of the slots
asked for that go back or past their bytes; an index of a dictionary
outside it in a slot asked for that is not `NULL`; a `Boolean` buffer
shorter than its bit offset and the slots; `FixedSizeBinary` bytes
shorter than the size times the slots; or a buffer of bytes for writing
shorter than the strings written, SHALL be refused before any value is
converted, its output unchanged, as an internal error, `XX000`. A write
SHALL check in three steps: the shape of what it reads, the slots asked
within the column and the bitmap and the buffers that cover them,
`XX000`; then the size of its values over the slots asked, `54000`; then
the shape of its output buffers, `XX000`. A broken shape SHALL be the
only error a call meets that its plan does not name.

#### Scenario: Every broken shape refused
- **WHEN** a read and a write of every plan are called over buffers of
  each broken shape the requirement lists, the values of each write
  within the size its offsets hold
- **THEN** each call gives `XX000`, its output unchanged, and none
  panics
- **AND** a write past the size with a slot asked past the column gives
  `XX000`, and one past the size with an output buffer too short gives
  `54000`
- **Verified by:** pending

### Requirement: The types of each side
The crate SHALL know these PostgreSQL types, by their object
identifiers and type modifiers: `boolean`, `smallint`, `integer`,
`bigint`, `real`, `double precision`, `date`, `text` and `bytea` with
the modifier -1; `timestamp` and `timestamptz` with the modifier -1, no
precision, or 6, microseconds; and `varchar` with the modifier -1, no
length, or a length from 1 to 10 485 760 plus 4, as PostgreSQL stores
it. It SHALL know these Arrow types: `Boolean`, the signed integers of 8
to 64 bits, the unsigned of 8 to 32, `Float32`, `Float64`, `Date32`,
`Timestamp` in seconds, milliseconds and microseconds, `Utf8`,
`LargeUtf8`, `Binary`, `LargeBinary`, `FixedSizeBinary` of a size of 0
or more, and a `Dictionary` of any of them, its indexes of any integer
type Arrow allows. A `Timestamp` whose time zone is absent or empty
SHALL be one without a time zone; any other time zone, whatever its
text, SHALL mean an instant in UTC. Any other type or modifier, a
`Dictionary` of a `Dictionary` among them, SHALL be refused, `0A000`.

#### Scenario: Types outside the sets
- **WHEN** plans are asked for `UInt64`, `Decimal128`, `Timestamp` in
  nanoseconds, `List`, `Struct`, a `Dictionary` of a `Dictionary`, a
  `Dictionary` whose indexes are `Utf8` and one whose indexes are
  `Float32`, a
  `FixedSizeBinary` of a negative size, `numeric`, `interval`, `uuid`,
  `timestamp(0)`, `timestamptz(3)`, `varchar` with the modifiers 4 and
  10 485 765, and `integer` with a modifier
- **THEN** each is refused with `0A000`, and `timestamp(6)`,
  `timestamptz(6)` and `varchar` with the modifiers 5 and 10 485 764
  are accepted
- **Verified by:** pending

### Requirement: A read plan
A read plan SHALL be made from an Arrow type, a PostgreSQL type and the
column's options: a **unit** that reads a bare integer as days since
1970-01-01, or as seconds, milliseconds or microseconds since
1970-01-01 00:00:00; the **treatment** of `Binary` and `LargeBinary`
as text, **checked** as UTF-8 or **trusted**; and whether the server's
encoding is UTF-8. It SHALL check, in order: the types, `0A000`; the
options, each on the Arrow type or a `Dictionary`'s values, the unit of
days on an integer of at most 32 bits and a unit of time on a 64-bit
integer, the treatment on `Binary` and `LargeBinary` into `text` or
`varchar`, and nothing else, `22023`; the pair, as the unit reads it,
`42804`; the size of a `FixedSizeBinary`, `54000`; and the server's
encoding for a plan into `text` or `varchar`, `0A000`. It SHALL accept
these pairs and no other:

- an integer into an integer type that holds every value of it, the
  unsigned into one wider than their bits;
- `Float32` into `real` or `double precision`, `Float64` into `double
  precision`; `Boolean` into `boolean`;
- `Date32`, or an integer with the unit of days, into `date`;
- `Timestamp` without a time zone into `timestamp`, with one into
  `timestamptz`, and a 64-bit integer with a unit of time into either;
- `Utf8` and `LargeUtf8` into `text` or `varchar`; `Binary` and
  `LargeBinary` into `bytea`, into `text` or `varchar` checked, and into
  `text` or `varchar` without a length trusted; `FixedSizeBinary` into
  `bytea`;
- a `Dictionary` as its values.

A plan SHALL name a failure of a value only where a value of its Arrow
type can fail: out of range, where the Arrow type holds values the
PostgreSQL type does not; from `Utf8` and `LargeUtf8`, which Arrow's
format requires to be UTF-8 and the crate takes so, a zero byte; from
checked bytes also bytes that are not UTF-8; the length of a `varchar`;
and the size, for strings and bytes of variable length. Trusted bytes
SHALL name none but the size.

#### Scenario: ClickBench's columns
- **WHEN** read plans are made for `Int16` into `smallint`, `UInt16`
  with the unit of days into `date`, `Int64` with the unit of seconds
  into `timestamp`, `Int64` into `bigint`, and `Binary` checked into
  `text`
- **THEN** each is accepted; the first, the second and the fourth name
  no failure of a value, the third a value out of range, and the last
  bytes that are not UTF-8, a zero byte and the size
- **Verified by:** pending

#### Scenario: Every pair accepted
- **WHEN** a read plan is made for each pair of the list: every integer
  into every type that holds it, `Float32` into both floating-point
  types, `Boolean`, `Date32` and integers with days, `Timestamp` in each
  unit with no time zone, an empty one, `UTC`, `Europe/Paris` and
  `+03:00`, and `Int64` with each unit of time into `timestamp` and
  `timestamptz`, `Utf8` and `LargeUtf8` into `text`, `varchar` and
  `varchar(10)`, `Binary` and `LargeBinary` into `bytea`, checked into
  `text`, `varchar` and `varchar(10)` and trusted into `text` and
  `varchar`, `FixedSizeBinary` into `bytea`, and a `Dictionary` of
  `Utf8`, of `Binary` and of `Int32` with the unit of days, with indexes
  of `Int8`, `Int32` and `UInt64`
- **THEN** each is accepted, the empty time zone as none and every other
  as UTC, and each names the failures the requirement gives its Arrow
  type
- **Verified by:** pending

#### Scenario: Pairs refused, in order
- **WHEN** read plans are asked for a `Decimal128` into `date` with a
  unit of days; a unit of days on a `Utf8` into `date`, on a `Date32`
  into `date` and on a `Utf8` into `integer`; a unit of seconds on an
  `Int32` into `timestamp`; a treatment on a `Utf8` into `text` and on
  `Binary` into `bytea`; `Int64` into `smallint`, `UInt32` into
  `integer`, `Float64` into `real`, `Utf8` into `date`, `Timestamp` with
  a time zone into `timestamp` and without one into `timestamptz`,
  `Int32` with the unit of days into `integer`, `Binary` into `text`
  without a treatment, and trusted `Binary` into `varchar(10)`; a
  `FixedSizeBinary` of 1 073 741 820 bytes into `bytea` and into `text`;
  and, on a server whose encoding is not UTF-8, `Utf8` into `text`,
  `Binary` into `text` without a treatment, and a unit of days on a
  `Utf8` into `text`
- **THEN** the `Decimal128` is refused with `0A000`, its type checked
  before its unit, the units and the treatments with `22023`, the units
  before the pairs, the pairs with `42804`; the `FixedSizeBinary` into
  `bytea` with `54000` and into `text` with `42804`, its pair checked
  before its size; and on that server `Utf8` into `text` with `0A000`,
  but `Binary` without a treatment with `42804` and the unit with
  `22023`, both checked before the encoding
- **Verified by:** pending

### Requirement: Values read
The conversion of a column's slots through a read plan SHALL give the
representations of the slots its caller's mask asks for, each into the
output's place of its slot, each value's representation in PostgreSQL
being: an integer as itself, a day as days since 2000-01-01, a time as
microseconds since 2000-01-01 00:00:00, a floating-point number with its
bits or widened to the same value, bytes and strings as they are, and a
`Boolean` from Arrow's bits at their offset. A slot SHALL be `NULL`
where the column's bitmap says so, and, in a `Dictionary`, also where
the value it refers to is `NULL` by the dictionary's own bitmap; a
`NULL` slot SHALL never fail, nor a value of a dictionary that no slot
asked for refers to, and the index of a `NULL` slot or of a slot not
asked for SHALL not be read. A value SHALL fail only in a way its plan
names, with the kinds of the requirement "Errors, never panics"; the
conversion SHALL stop at the first slot asked for that fails and report
it, having given the slots before it their representations. A
`varchar`'s length SHALL be counted in characters, and a string longer
SHALL fail even where only spaces run past it, which PostgreSQL's input
would cut off: the crate never shortens a value. For an array of
integers, floating-point numbers, days, times or `Boolean`, not a
`Dictionary`, the conversion MAY write the output's place of a slot not
asked for or `NULL`, so that its loop can convert a run of slots at
once: with the slot's representation where the slot is not `NULL` and
its value converts without a failure, so that a place a caller filled
before keeps its value, and otherwise with a value of no meaning; it
SHALL raise no failure of such a slot. The output's place of a `NULL`
slot SHALL have no meaning after a read. Strings and bytes,
`FixedSizeBinary` among them, SHALL be given for the slots asked alone.

#### Scenario: Days and times at the edges of each unit
- **WHEN** these are read: from `Date32` and from days in an `Int32`,
  the first and the last day PostgreSQL holds and one past each; days
  in a `UInt32` of 2 147 483 648 and 4 294 967 295, and in a `UInt16`
  of 65 535; from seconds and milliseconds, the first and the last
  whole unit PostgreSQL holds and one past each; and from microseconds,
  the first one PostgreSQL holds, one before it, and the largest 64-bit
  integer
- **THEN** the values within the range give their representations, the
  `UInt16` and the largest 64-bit integer of microseconds among them,
  and the others fail with `22008`
- **Verified by:** pending

#### Scenario: Strings checked
- **WHEN** strings are read into `varchar(3)` from checked bytes: one
  that is not UTF-8 and one that holds a zero byte, each longer than
  three characters, three characters of two bytes each, four characters,
  and three letters and a space; the same but the one that is not UTF-8
  from `Utf8`; the same from trusted bytes into `text`; and a string of
  1 073 741 820 bytes of zero bytes from checked bytes into `text`
- **THEN** from checked bytes the one that is not UTF-8 and the zero
  byte fail with `22021`, not `22001`, the three characters of six bytes
  pass, and the four characters and the letters with the space fail with
  `22001`; from `Utf8` the zero byte fails with `22021` and the others
  as from checked bytes; from trusted bytes all pass; and the large one
  fails with `54000`, its size checked before its zero bytes
- **Verified by:** pending

#### Scenario: Numbers widened
- **WHEN** the least and the greatest value of every integer type are
  read into every integer type that holds them, `UInt8` 255 into
  `smallint`, `UInt16` 65 535 into `integer` and `UInt32` 4 294 967 295
  into `bigint` among them, and `Float32` values, among them the
  smallest subnormal, the largest finite, both zeros, both infinities
  and `NaN`, into `double precision`
- **THEN** each integer gives its own value, and each `Float32` the
  `double precision` of the same value, its sign kept, `NaN` a `NaN`
- **Verified by:** pending

#### Scenario: Slots asked, NULL slots and dictionaries
- **WHEN** a column whose `NULL` slots hold an out-of-range time and
  bytes that are not UTF-8, a column without a bitmap, a column whose
  bitmap starts at a bit offset of 5, a slot not asked for that holds
  an out-of-range time, a `Boolean` column read from a bit offset of 3,
  and a `Dictionary` whose unreferenced value is not UTF-8, whose
  referenced value is `NULL` by the dictionary's bitmap over bytes that
  are not UTF-8, and whose `NULL` slot and slot not asked for hold
  indexes past its end, are read; and then a column whose fifth slot
  asked for is out of range
- **THEN** the `NULL` slots give `NULL`, the slot not asked for raises
  no failure, the booleans and the referenced values give their
  representations, nothing fails, and the last call reports the fifth
  slot, the four before it converted
- **Verified by:** pending

#### Scenario: A failure found within a word
- **WHEN** a column of 100 slots of seconds in an `Int64`, its last word
  partial, is read into `timestamp`, its first failing slot asked for in
  the second word, and before that slot, in the same word, a `NULL` slot
  and a slot not asked for that hold the least and the greatest 64-bit
  integers
- **THEN** the failure reports the slot asked for, the slots asked
  before it give their representations, and nothing panics
- **Verified by:** pending

### Requirement: A write plan
A write plan SHALL be made from a PostgreSQL type and whether the
server's encoding is UTF-8, and SHALL give the Arrow type the column
becomes, the caller giving the field its name and whether it holds
`NULL`: `boolean` as `Boolean`; `smallint`, `integer` and `bigint` as
`Int16`, `Int32` and `Int64`; `real` and `double precision` as `Float32`
and `Float64`; `date` as `Date32`; `timestamp` as `Timestamp` in
microseconds without a time zone, and `timestamptz` with the time zone
`UTC`; `text` and `varchar` as `Utf8`; `bytea` as `Binary`. The plans of
`date`, `timestamp` and `timestamptz` SHALL name one failure of a value,
out of range: an `infinity` or `-infinity`, which Arrow cannot hold, a
representation outside PostgreSQL's range, and, for the two times, a
time past the last microsecond since 1970 that a 64-bit integer holds,
in PostgreSQL's last thirty years. The plans of `text`, `varchar` and
`bytea` SHALL name one, the size of a call's values; the others none.
A plan of `text` or `varchar` when the server's encoding is not
UTF-8 SHALL be refused, `0A000`.

#### Scenario: Types of the columns written
- **WHEN** a write plan is made for each PostgreSQL type the crate
  knows, and for `text` on a server whose encoding is not UTF-8
- **THEN** each gives the type listed above; the plans of `date`,
  `timestamp` and `timestamptz` name a value out of range, those of
  `text`, `varchar` and `bytea` the size, the others no failure; and
  the plan of `text` on that server is refused with `0A000`
- **Verified by:** pending

### Requirement: Values written
The conversion of a column's representations through a write plan SHALL
convert only the slots its caller's mask asks for, one Arrow value a
slot asked, in their order, and SHALL give the Arrow values its type
holds, into the caller's buffers: a day as days since 1970-01-01, a time
as microseconds since 1970-01-01 00:00:00, a `boolean` as a bit, packed
from the first bit of its buffer, a string or bytes as their bytes with
the offsets of `Utf8` and `Binary`, the others as they are, and `NULL`
for a slot the caller marks `NULL`, which never fails. The caller's
representations of `boolean`, `text`, `varchar` and `bytea` SHALL be
taken as values of their types, a `boolean` byte other than 0 as true.
It SHALL fail only as its plan names: out of range, `22008`, stopping at
the first slot that fails and reporting it, having written the slots
asked before it; and the size, `54000`, checked over the slots asked
before any value is written, the call refused at the first slot whose
bytes pass the limit.

#### Scenario: Values, infinities and the last times written
- **WHEN** the first and the last day PostgreSQL holds, a date before
  the first and one past the last, the first microsecond PostgreSQL
  holds and one before it, the last one a 64-bit integer since 1970
  holds and the one after it, a time past the last PostgreSQL holds,
  `infinity` and `-infinity`, and a `NULL`, are written as `date`,
  `timestamp` and `timestamptz`, with a slot not asked for that holds
  `infinity`; `boolean` bytes of 0, 1 and 2; and three `bytea` values
  of 1 073 741 819 bytes each in one call, into a buffer of bytes
  shorter than them
- **THEN** the days and times that fit give their Arrow values, packed,
  those after the slot not asked for following those before it with no
  gap and the bitmap packed the same way, the `NULL` gives `NULL`, the
  slot not asked for raises no failure, and the others fail with
  `22008`, each reported by its slot, the slots before it written; the
  booleans give false, true and true; and the call of `bytea` fails with
  `54000` at its third slot, not `XX000`, nothing written
- **Verified by:** pending

### Requirement: The round trip
A value of a PostgreSQL type the crate knows, valid for its type (a
`boolean` of 0 or 1, a string that is UTF-8 without a zero byte and
within its length), written through its write plan without a failure
and read back through the read plan of the type the write plan gave,
SHALL come back with the same representation, bit for bit.

#### Scenario: Every type written and read back
- **WHEN** values of every type, drawn at random over their whole
  ranges with their edges, `NaN` among the floating-point ones, and
  `NULL`, are written and read back, `timestamp(6)` and `timestamptz(6)`
  among the types
- **THEN** each one written comes back with the same representation
- **Verified by:** pending

#### Scenario: Arrow's own arrays
- **WHEN** arrays of every Arrow type the crate takes, built by Apache
  Arrow's `arrow-array`, sliced and not, with and without a bitmap, are
  read, and the arrays the crate writes are given to `arrow-array`
- **THEN** the values read equal the values `arrow-array` gives, and
  `arrow-array` accepts each array written as valid and gives the
  values written
- **Verified by:** pending

### Requirement: The bounds of a comparison
For a read plan of integers, of days or of times, the crate SHALL turn a
PostgreSQL constant of the plan's type, or for a plan of integers of any
of `smallint`, `integer` and `bigint`, and a comparison (`=`, `<>`, `<`,
`<=`, `>`, `>=`) into a **test**, and SHALL apply the test to a column's
stored values with their bitmap, giving the slots that pass as a mask: a
slot that is not `NULL` passes exactly when the comparison of its value,
its exact count of the unit in PostgreSQL's terms, with the constant is
true, for every stored value, also one the conversion would refuse as
out of range. The test SHALL be a comparison with a bound in the stored
unit, which may have another operator than the comparison, or "every
value" or "no value". A constant of `infinity` SHALL be above every
stored value and `-infinity` below; a `NULL` constant SHALL give "no
value"; a `NULL` slot SHALL pass no test. A test SHALL also answer, for
a closed range of stored values, such as the least and the greatest
values of a part of a file, whether no value in it passes, some may, or
every value does, exactly as the test of each value in the range would;
the answer is of the values that are not `NULL`, so that a caller keeps
every row of a part on "every value" only where the part holds no
`NULL`. A range whose least value is above its greatest, as the
statistics of a damaged file may give, SHALL get "some may", which rules
nothing out. For a `Dictionary`, the test SHALL apply to the
dictionary's values, and the caller SHALL take each slot's answer by its
index. Bounds asked of any other plan SHALL be refused, `0A000`.

#### Scenario: Bounds against the conversion
- **WHEN** constants are drawn at random, with the edges of the range,
  times that are not whole seconds, the infinities and `NULL`, and
  compared by each comparison with stored values in days, seconds,
  milliseconds and microseconds, and with stored integers of every
  width, signed and unsigned, such as `UInt8` into `smallint` against
  256 and against a negative constant, and `Int16` into `smallint`
  against the `bigint` constants 40 000 and -40 000, and the values of a
  `Dictionary` of `Int32` with the unit of days, through the test and
  through an exact count in 128 bits of each stored value, over columns
  with `NULL` slots
- **THEN** both give the same result for every stored value, those out
  of PostgreSQL's range among them, and no `NULL` slot passes
- **Verified by:** pending

#### Scenario: Ranges of stored values
- **WHEN** ranges of stored values are drawn at random, with ranges of
  one value, ranges at the ends of the stored type, ranges that hold
  the constant and ranges whose least value is above their greatest,
  and each test is asked of each range
- **THEN** its answer equals the one the test of every value of the
  range gives, by an exact count in 128 bits: no value, some, or every
  value; and a range turned the wrong way gets "some may"
- **Verified by:** pending

#### Scenario: Plans without bounds
- **WHEN** bounds are asked of read plans into `real`, `text` and
  `bytea`
- **THEN** each is refused with `0A000`
- **Verified by:** pending
