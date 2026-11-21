# Spec Delta

## Purpose

`datum-arrow` is the boundary between PostgreSQL's types and the types
of Apache Arrow, a form of columns in memory into which Apache Arrow's
implementations read files such as Parquet and from which they write
them. A **read plan** says how the values of an Arrow column become
values of a PostgreSQL type, and a **write plan** how the values of a
PostgreSQL column become an Arrow column. A plan is made once for a
column, before any value is converted: it refuses a pair of types it
cannot convert, and it names every way a value may still fail. The
conversions work on PostgreSQL's **representations**, the forms a value
has inside the server, as a `Datum` or a value's bytes hold it, and
need no PostgreSQL to run. A **slot** is the place of one value in an
Arrow array, and a **mask** is a set of slots, 64-bit words of a bit a
slot. ClickBench, a benchmark of analytic queries, is the first user of
the crate, through a reader of its Parquet files.

The crate's documentation holds the rules: the types, the pairs, the
order of the checks, the representations, the ranges and the limits.
Its page, the crate's
[README](../../../crates/datum-arrow/README.md), shows the whole with
examples. Each requirement here says what is promised and links to the
items whose documentation holds the rest. The requirements go from the
crate as a whole and its errors to the types it knows, the two plans,
the values read and written, the round trip and the bounds of a
comparison in a file's own unit; [design.md](design.md) explains the
whole and the reasons.

## ADDED Requirements

### Requirement: A crate of its own
The boundary SHALL be the Rust crate `datum-arrow`, which depends on no
crate of Tessera and on no code of PostgreSQL, and of Apache Arrow on
`arrow-schema` alone outside its tests, and SHALL build, pass its tests
and pack alone. Its documentation SHALL be complete by itself: every
public item SHALL say what it is for, what it takes and gives, the
errors it returns, and the rules and limits of the API.

#### Scenario: The crate packs alone
- **WHEN** the crate is packed, as for publishing, and its dependencies
  outside its tests are listed
- **THEN** its library builds from the packed files and the crates it
  names, and the list holds `arrow-schema` alone
- **Verified by:**
  `.github/workflows/ci.yml::cargo publish --dry-run --locked -p datum-arrow`;
  `.github/workflows/ci.yml::cargo tree --locked -p datum-arrow -e normal`

#### Scenario: Every public item documented
- **WHEN** the crate is built with documentation required of every
  public item, and its documentation is built with warnings as errors
- **THEN** both build, and the examples of the documentation run
- **Verified by:** `crates/datum-arrow/src/lib.rs::#![deny(missing_docs)]`;
  `.github/workflows/ci.yml::cargo doc --locked -p datum-arrow --no-deps`;
  `.github/workflows/ci.yml::cargo test --locked -p datum-arrow --doc`

### Requirement: Errors, never panics
No call SHALL panic, whatever its input. A refusal of a plan or a
failure of a value SHALL be an
[`Error`](../../../crates/datum-arrow/src/error.rs) of one of the kinds
[`ErrorKind`](../../../crates/datum-arrow/src/error.rs) lists, each
with the SQLSTATE a PostgreSQL server raises for it, and a failure of a
value SHALL give the slot it came from. Where a value meets several
failures, they SHALL be checked in the order
[`ReadPlan`](../../../crates/datum-arrow/src/read.rs) gives, and only
the first reported.

#### Scenario: No panic on any input
- **WHEN** plans are made from every pair of known types with every
  option, and reads, bounds and writes are called through each over
  slots of every bit pattern
- **THEN** every call returns its result or an error of the listed
  kinds, and none panics
- **Verified by:**
  `crates/datum-arrow/src/read/tests.rs::no_panic_on_any_input`;
  `crates/datum-arrow/src/error.rs::every_kind_has_its_sqlstate`

### Requirement: Calls of a broken shape
A call whose buffers break the shape of its arrays or of the call SHALL
be refused before any value is converted, its output unchanged, as an
internal error, `XX000`, in the cases
[`ReadPlan::read`](../../../crates/datum-arrow/src/read.rs),
[`WritePlan::write`](../../../crates/datum-arrow/src/write.rs) and
[`Test::apply`](../../../crates/datum-arrow/src/bounds.rs) list. A
write SHALL check its input's shape, then the size of its values, then
its output's shape. A broken shape SHALL be the only error a call meets
that its plan does not name.

#### Scenario: Every broken shape refused
- **WHEN** a read and a write of every plan are called over buffers of
  each broken shape their documentation lists, the values of each
  write within the size its offsets hold
- **THEN** each call gives `XX000`, its output unchanged, and none
  panics
- **AND** a write past the size with a slot asked past the column gives
  `XX000`; one past the size with an output buffer too short gives
  `54000`; and one of exactly the size the offsets hold, into an output
  too short, gives `XX000`
- **Verified by:**
  `crates/datum-arrow/src/read/tests.rs::every_broken_shape_refused`;
  `crates/datum-arrow/src/read/tests.rs::broken_shapes_of_a_dictionary_and_of_the_output`;
  `crates/datum-arrow/src/write/tests.rs::broken_shapes_of_a_write`;
  `crates/datum-arrow/src/write/tests.rs::bytes_past_the_offsets_fail_before_anything_is_written`;
  `crates/datum-arrow/src/write/tests.rs::bytes_of_one_call_up_to_what_offsets_hold`

### Requirement: The types of each side
The crate SHALL know the PostgreSQL types
[`PgType`](../../../crates/datum-arrow/src/pg.rs) lists, by their
object identifiers and type modifiers, and the Arrow types
[`ArrowType`](../../../crates/datum-arrow/src/arrow.rs) lists. Any
other type or modifier SHALL be refused, `0A000`.

#### Scenario: Types outside the sets
- **WHEN** plans are asked for `UInt64`, `Decimal128`, `Timestamp` in
  nanoseconds, `List`, `Struct`, a `Dictionary` of a `Dictionary`, a
  `Dictionary` whose indexes are `Utf8` and one whose indexes are
  `Float32`, a `FixedSizeBinary` of a negative size, `numeric`,
  `interval`, `uuid`, `timestamp(0)`, `timestamptz(3)`, `varchar` with
  the modifiers 4 and 10 485 765, and `integer` with a modifier
- **THEN** each is refused with `0A000`, and `timestamp(6)`,
  `timestamptz(6)` and `varchar` with the modifiers 5 and 10 485 764
  are accepted
- **Verified by:**
  `crates/datum-arrow/src/pg.rs::types_outside_the_set`;
  `crates/datum-arrow/src/pg.rs::every_known_type_and_its_modifiers`;
  `crates/datum-arrow/src/pg.rs::a_varchar_by_its_length`;
  `crates/datum-arrow/src/arrow.rs::types_outside_the_set`;
  `crates/datum-arrow/src/arrow.rs::timestamps_by_unit_and_zone`

### Requirement: A read plan
A read plan SHALL be made from an Arrow type, a PostgreSQL type and the
column's [`ReadOptions`](../../../crates/datum-arrow/src/read.rs). It
SHALL accept the pairs the table of
[`ReadPlan`](../../../crates/datum-arrow/src/read.rs) lists and no
other, checking in the order
[`ReadPlan::new`](../../../crates/datum-arrow/src/read.rs) gives, and
SHALL name, in
[`ReadPlan::failures`](../../../crates/datum-arrow/src/read.rs), a
failure of a value only where a value of its Arrow type can meet it.

#### Scenario: ClickBench's columns
- **WHEN** read plans are made for `Int16` into `smallint`, `UInt16`
  with the unit of days into `date`, `Int64` with the unit of seconds
  into `timestamp`, `Int64` into `bigint`, and `Binary` checked into
  `text`
- **THEN** each is accepted; the first, the second and the fourth name
  no failure of a value, the third a value out of range, and the last
  bytes that are not UTF-8, a zero byte and the size
- **Verified by:**
  `crates/datum-arrow/src/read/tests.rs::clickbench_columns`

#### Scenario: Every pair accepted
- **WHEN** a read plan is made for each pair of the table: every
  integer into every type that holds it, `Float32` into both
  floating-point types, `Boolean`, `Date32` and integers with days,
  `Timestamp` in each unit with no time zone, an empty one, `UTC`,
  `Europe/Paris` and `+03:00`, and `Int64` with each unit of time into
  `timestamp` and `timestamptz`, `Utf8` and `LargeUtf8` into `text`,
  `varchar` and `varchar(10)`, `Binary` and `LargeBinary` into `bytea`,
  checked into `text`, `varchar` and `varchar(10)` and trusted into
  `text` and `varchar`, `FixedSizeBinary` into `bytea`, and a
  `Dictionary` of `Utf8`, of `Binary` and of `Int32` with the unit of
  days, with indexes of `Int8`, `Int32` and `UInt64`
- **THEN** each is accepted, the empty time zone as none and every other
  as UTC, and each names the failures its documentation gives its Arrow
  type
- **Verified by:**
  `crates/datum-arrow/src/read/tests.rs::every_pair_accepted`

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
- **Verified by:**
  `crates/datum-arrow/src/read/tests.rs::pairs_refused_in_order`

### Requirement: Values read
The conversion of a column's slots through a read plan,
[`ReadPlan::read`](../../../crates/datum-arrow/src/read.rs), SHALL give
the representations of the slots its caller's mask asks for, `NULL`
where a bitmap says so, and SHALL fail only in a way its plan names,
stopping at the first slot asked for that fails, having given the slots
before it their representations. A `NULL` slot and a slot not asked
for SHALL never fail. Its documentation gives the representations, the
ranges, what it may write in a place of a slot not asked for, and what
it leaves unread.

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
- **Verified by:**
  `crates/datum-arrow/src/read/tests.rs::days_and_times_at_the_edges_of_each_unit`;
  `crates/datum-arrow/src/read/tests.rs::seconds_of_every_bit_pattern`;
  `crates/datum-arrow/src/proofs.rs::seconds_read_exactly_in_postgresql_s_range`

#### Scenario: Strings checked
- **WHEN** strings are read into `varchar(3)` from checked bytes: one
  that is not UTF-8 and one that holds a zero byte, each longer than
  three characters, three characters of two bytes each, four characters,
  and three letters and a space; the same but the one that is not UTF-8
  from `Utf8`; the same from trusted bytes into `text`; a string of
  1 073 741 820 zero bytes from checked bytes into `text`; and one of
  1 073 741 819 bytes into `bytea`
- **THEN** from checked bytes the one that is not UTF-8 and the zero
  byte fail with `22021`, not `22001`, the three characters of six bytes
  pass, and the four characters and the letters with the space fail with
  `22001`; from `Utf8` the zero byte fails with `22021` and the others
  as from checked bytes; from trusted bytes all pass; the large one
  fails with `54000`, its size checked before its zero bytes; and the
  one of the largest size passes
- **Verified by:**
  `crates/datum-arrow/src/read/tests.rs::strings_checked`;
  `crates/datum-arrow/src/read/tests.rs::a_string_too_large_fails_before_its_bytes_are_checked`;
  `crates/datum-arrow/src/read/tests.rs::bytes_of_every_pattern`;
  `crates/datum-arrow/src/proofs.rs::checked_text_is_refused_exactly`

#### Scenario: Numbers widened
- **WHEN** the least and the greatest value of every integer type are
  read into every integer type that holds them, `UInt8` 255 into
  `smallint`, `UInt16` 65 535 into `integer` and `UInt32` 4 294 967 295
  into `bigint` among them, and `Float32` values, among them the
  smallest subnormal, the largest finite, both zeros, both infinities
  and `NaN`, into `double precision`
- **THEN** each integer gives its own value, and each `Float32` the
  `double precision` of the same value, its sign kept, `NaN` a `NaN`
- **Verified by:**
  `crates/datum-arrow/src/read/tests.rs::numbers_widened`;
  `crates/datum-arrow/src/read/tests.rs::dense_families_against_the_code_of_one_value`

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
- **Verified by:**
  `crates/datum-arrow/src/read/tests.rs::slots_asked_null_slots_and_dictionaries`;
  `crates/datum-arrow/src/read/tests.rs::dictionaries_against_the_code_of_one_value`

#### Scenario: A failure found within a word
- **WHEN** a column of 100 slots of seconds in an `Int64`, its last word
  partial, is read into `timestamp`, its first failing slot asked for in
  the second word, and before that slot, in the same word, a `NULL` slot
  and a slot not asked for that hold the least and the greatest 64-bit
  integers
- **THEN** the failure reports the slot asked for, the slots asked
  before it give their representations, and nothing panics
- **Verified by:**
  `crates/datum-arrow/src/read/tests.rs::a_failure_found_within_a_word`

### Requirement: A write plan
A write plan,
[`WritePlan::new`](../../../crates/datum-arrow/src/write.rs), SHALL be
made from a PostgreSQL type and whether the server's encoding is UTF-8,
SHALL give the Arrow type the column becomes,
[`WritePlan::data_type`](../../../crates/datum-arrow/src/write.rs), and
SHALL name the failures of its values,
[`WritePlan::failures`](../../../crates/datum-arrow/src/write.rs), as
their documentation gives them.

#### Scenario: Types of the columns written
- **WHEN** a write plan is made for each PostgreSQL type the crate
  knows, and for `text` on a server whose encoding is not UTF-8
- **THEN** each gives the type its documentation lists; the plans of
  `date`, `timestamp` and `timestamptz` name a value out of range, those
  of `text`, `varchar` and `bytea` the size, the others no failure; and
  the plan of `text` on that server is refused with `0A000`
- **Verified by:**
  `crates/datum-arrow/src/write/tests.rs::types_of_the_columns_written`

### Requirement: Values written
The conversion of a column's representations through a write plan,
[`WritePlan::write`](../../../crates/datum-arrow/src/write.rs), SHALL
convert only the slots its caller's mask asks for, one Arrow value a
slot asked, in their order, into the caller's buffers, `NULL` for a
slot the caller marks so, which never fails. It SHALL fail only as its
plan names: out of range, stopping at the first slot that fails, having
written the slots asked before it; and the size, checked over the slots
asked before any value is written.

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
- **Verified by:**
  `crates/datum-arrow/src/write/tests.rs::values_infinities_and_the_last_times_written`;
  `crates/datum-arrow/src/write/tests.rs::the_values_before_a_failure_are_written`;
  `crates/datum-arrow/src/write/tests.rs::bytes_past_the_offsets_fail_before_anything_is_written`;
  `crates/datum-arrow/src/proofs.rs::dates_written_in_range_and_read_back`;
  `crates/datum-arrow/src/proofs.rs::times_written_in_range_and_read_back`

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
- **Verified by:**
  `crates/datum-arrow/src/write/tests.rs::every_type_written_and_read_back`

#### Scenario: Arrow's own arrays
- **WHEN** arrays of every Arrow type the crate takes, built by Apache
  Arrow's `arrow-array`, sliced and not, with and without a bitmap, are
  read, and the arrays the crate writes are given to `arrow-array`
- **THEN** the values read equal the values `arrow-array` gives, and
  `arrow-array` accepts each array written as valid and gives the
  values written
- **Verified by:**
  `crates/datum-arrow/src/write/tests.rs::arrow_s_own_arrays`

### Requirement: The bounds of a comparison
For a read plan of integers, of days or of times, the crate SHALL turn a
constant and a comparison into a
[`Test`](../../../crates/datum-arrow/src/bounds.rs) in the stored unit,
[`ReadPlan::test`](../../../crates/datum-arrow/src/bounds.rs), that a
slot not `NULL` passes exactly when the comparison of its exact count in
PostgreSQL's terms with the constant is true, for every stored value,
also one the conversion would refuse. A test SHALL answer for a closed
range of stored values,
[`Test::range`](../../../crates/datum-arrow/src/bounds.rs), exactly as
the test of each value in it would, and a range turned the wrong way
SHALL get "some may". Bounds asked of any other plan SHALL be refused,
`0A000`.

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
- **Verified by:**
  `crates/datum-arrow/src/bounds/tests.rs::bounds_against_the_conversion`;
  `crates/datum-arrow/src/bounds/tests.rs::a_test_applied_to_columns_of_every_width`;
  `crates/datum-arrow/src/bounds/tests.rs::bounds_of_clickbench_s_clauses`;
  `crates/datum-arrow/src/proofs.rs::bounds_of_integers_are_exact`;
  `crates/datum-arrow/src/proofs.rs::bounds_of_microseconds_are_exact`

#### Scenario: Ranges of stored values
- **WHEN** ranges of stored values are drawn at random, with ranges of
  one value, ranges at the ends of the stored type, ranges that hold
  the constant and ranges whose least value is above their greatest,
  and each test is asked of each range
- **THEN** its answer equals the one the test of every value of the
  range gives, by an exact count in 128 bits: no value, some, or every
  value; and a range turned the wrong way gets "some may"
- **Verified by:**
  `crates/datum-arrow/src/bounds/tests.rs::ranges_of_stored_values`;
  `crates/datum-arrow/src/proofs.rs::answers_for_a_range_are_exact`

#### Scenario: Plans without bounds
- **WHEN** bounds are asked of read plans into `real`, `text` and
  `bytea`
- **THEN** each is refused with `0A000`
- **Verified by:**
  `crates/datum-arrow/src/bounds/tests.rs::plans_without_bounds`
