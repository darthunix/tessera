# datum-arrow: how it is built

A reader of Parquet files into PostgreSQL, or a writer of PostgreSQL's
rows into them, crosses one boundary on every value: the one between
PostgreSQL's types and the types of Apache Arrow. Arrow is a form of
columns in memory; Apache Arrow's implementations read Parquet files
into it and write them from it. The crate `datum-arrow` holds that
boundary: which PostgreSQL type an Arrow column may become, and the
reverse, and the conversion of each value. It is a crate of the
workspace meant for use outside Tessera. Its documentation holds the
rules of its API, the types, the pairs, the ranges, the order of the
checks and the limits, and its page shows the whole with examples
([README](../../../crates/datum-arrow/README.md)); its spec says what it
promises ([spec.md](spec.md)). This page says how it is built and why.

## The problem

The two sides hold the same values in forms that differ in small ways,
and each difference loses data when it is missed.

- **Epochs.** Arrow counts a date in days and a time in its unit since
  the first moment of 1970; PostgreSQL counts both since the first
  moment of 2000.
- **Units.** Arrow keeps a time in seconds, milliseconds, microseconds
  or nanoseconds; PostgreSQL keeps microseconds.
- **Ranges.** They overlap, but neither holds the other: a 64-bit count
  of microseconds since 1970 ends about thirty years before
  PostgreSQL's last `timestamp`, and a 64-bit count of seconds reaches
  far past it.
- **Infinities.** PostgreSQL has `infinity` and `-infinity` for dates
  and times; Arrow has none.
- **Text.** A PostgreSQL `text` is valid in the server's encoding and
  holds no zero byte; a `varchar(n)` counts its length in characters;
  no value passes about a gigabyte. Arrow's bytes promise none of this.
- **Integers without a mark.** A file may keep a date or a time as a
  bare integer, with nothing that says what it counts; ClickBench, a
  benchmark of analytic queries, keeps its dates as days in 16 bits and
  its times as seconds in 64.

A reader that converts values by itself repeats these rules, and a
writer repeats them again; the two drift apart. The crate keeps them in
one place, tested without a server.

## Goals and what they cost

- **One owner of the conversions.** Every conversion between Arrow's
  form and PostgreSQL's goes through the crate. The cost: a caller
  speaks the crate's types for its columns, and the crate follows
  Arrow's releases.
- **A plan checked once.** A column's conversion is a plan made once,
  before any value is converted. It refuses a pair of types it cannot
  convert, and it names every way a value may still fail, so that the
  work per value is the plan's checks and nothing more, and a caller
  that handles the errors the plan names has handled every value. The
  cost: the plan's rules are a closed list, and a new type is a change
  of the list.
- **PostgreSQL's own forms.** The conversions give and take the forms a
  value has inside the server, so that a caller fills a batch's column
  in one pass and raises the error PostgreSQL would raise. The cost: the
  crate depends on those forms, which PostgreSQL keeps across releases
  as part of its on-disk format.
- **Loops a compiler vectorizes.** A conversion over a batch runs as a
  loop with no branch per value. The cost: a read of a plain column of
  integers, floating-point numbers, days, times or booleans writes
  places of slots the caller did not ask for, and the API says what
  those places hold.
- **Exact bounds.** A clause such as `EventTime >= $1` is checked on the
  file's own stored integers, without converting a value, and gives
  exactly the rows the conversion and PostgreSQL's comparison would. The
  cost: the arithmetic of the bound is exact over every stored value,
  also those PostgreSQL cannot hold.
- **Proved where it can be.** The code of one value is proved for every
  input by Kani, and the loops over a batch are compared with it. The
  cost: the code of one value is kept apart from the loops, as their
  reference.

## The whole

```
  reading
  ───────
  Arrow arrays (the caller's buffers)
        │
        ▼
  ReadPlan::new(Arrow type, PostgreSQL type, options)
        │   refused: the error and its SQLSTATE
        ▼
  a plan ──► failures(): the ways a value may still fail
        │
        ├──► read(column, slots asked) ──► Datum words, or slices
        │                                  of the input's bytes
        └──► test(comparison, constant) ──► a test of stored values
                                            passes, range, apply

  writing
  ───────
  WritePlan::new(PostgreSQL type, encoding) ──► the Arrow type
        │
        ▼
  write(representations, slots asked) ──► Arrow buffers (the caller's)
```

A plan holds the pair of types, the options and the failures it allows.
A conversion takes a plan, a column's buffers, its bitmap of `NULL`
values and a mask of the slots the caller asks for. A read checks the
shape of its buffers first; a write checks the shape of its input, then
the size of its values, then the shape of its output, so that values
too large are refused as such though the output be short too
([`WritePlan::write`](../../../crates/datum-arrow/src/write.rs) gives
the steps). Then the conversion takes each slot asked, and stops at the
first slot asked that fails, with that slot and its error. The caller
owns every buffer; the crate allocates nothing in a conversion and
keeps nothing after it.

The modules follow this picture. `pg.rs` and `arrow.rs` know the two
sides' types; `column.rs` borrows Arrow's buffers; `read.rs` and
`write.rs` make the plans and run the conversions; `bounds.rs` makes the
tests of stored values; `calendar.rs` holds the epochs, the ranges and
the counts of days and time units, the arithmetic both directions and
the bounds share; `error.rs` and `failures.rs` the errors and the sets
of failures a plan names.

## A crate of its own

The crate depends on no crate of Tessera and on no code of PostgreSQL.
It knows PostgreSQL's types by their object identifiers, fixed for the
built-in types, and their representations, fixed in PostgreSQL's
on-disk format; it links no library of PostgreSQL and calls none. It
forbids `unsafe` code: all its memory comes as safe slices its caller
makes. So it builds, tests and packs alone, and serves any reader or
writer on either side, in Tessera or not.

Refused: the conversions in the C code of a reader of files, where a
writer would have to repeat them, and where nothing tests them without
a server.

## Arrow's types, not Parquet's schema

Apache Arrow's implementation already turns a Parquet column into an
Arrow array and back, by Parquet's physical types and its marks of
what a column holds. The crate speaks Arrow's `DataType`, through the
small crate `arrow-schema`, which it re-exports, so that a reader and a
writer over Parquet share one boundary, and so would one over any other
format Arrow reads. What a file loses, an integer that counts days or
seconds without a mark, comes back as the plan's **unit**, an option the
caller gives from what it knows of the file.

`arrow-schema` is the crate's only dependency outside its tests. Its
`DataType` is in the crate's public API, and Apache Arrow takes a new
major version several times a year, so a release of the crate works with
one major version of Arrow, and the snapshot of its public API changes
with each move.

Refused: a mapping from Parquet's physical types and marks, which a
writer through Arrow would have to repeat on its side.

## A plan checked once

A plan is made from the two types and the options, and it is refused
when the pair cannot be converted;
[`ReadPlan`](../../../crates/datum-arrow/src/read.rs) and
[`WritePlan`](../../../crates/datum-arrow/src/write.rs) give the pairs,
and [`ReadPlan::new`](../../../crates/datum-arrow/src/read.rs) the order
of its checks. An accepted plan names its failures, only those a value
of its Arrow type can meet: an integer widened into a type that holds
every value of it names none, nor does a day in 16 bits, which every
`date` holds; a day in 32 bits names a value out of range; a string
names its zero byte, checked bytes their UTF-8 too, a `varchar` its
characters, and a value of variable length its size. A conversion runs
only the checks its plan names.

The kinds of error are few and closed, and each gives the SQLSTATE a
PostgreSQL server raises in the same case, so that a caller inside a
server raises the error the server would. One kind is the caller's bug
rather than a value's: a call of a **broken shape**, whose buffers are
shorter than the slots asked, or whose offsets go back, is refused
before any value is converted, its output unchanged, as an internal
error. It is the only error a call meets that its plan does not name.

Where PostgreSQL's input would change a value, the crate refuses
instead. A `timestamp(p)` below microseconds rounds on input, so the
crate knows only the precisions that keep a microsecond. A `varchar(n)`
cuts the spaces past its length on input, so a string longer by spaces
fails here. A plan never changes a value: it converts it, or names how
it may fail.

Refused: checks per value without a plan, which every caller would
have to order and handle by itself, and an error found only on the
first bad value of a file, after rows were already given.

## PostgreSQL's representations

The conversions work on PostgreSQL's **representations**: a value of a
type the server passes by value as the 64-bit word of a `Datum`, as the
server's own functions make it, and a `text`, a `varchar` or a `bytea`
as its bytes without PostgreSQL's header of length. A read gives Datum
words for a type passed by value, so that a caller fills a batch's
column in one pass, and slices of the input's bytes for strings, so that
the caller copies each one once, into a value with its header, in memory
the server owns; a `FixedSizeBinary` is bytes too, read into `bytea`. A
write takes the same forms back.

Refused: the format of `COPY BINARY`, in network byte order with a
length before each value, which crates such as `arrow-pg` and `pgpq`
make for PostgreSQL's protocol; a value inside the server is not in it.
The crate's name says what it converts, the `Datum` values inside the
server; Apache Arrow's crates have a trait `Datum` of their own, a value
or an array given to a kernel, which is another thing.

## Slots, masks and dictionaries

A **slot** is the place of one value in an Arrow array. The slots a
call asks for come as a **mask**, 64-bit words of a bit a slot, the form
of a batch's rows. A slot is `NULL` where the array's bitmap says so;
its bytes have no meaning and it never fails. A `Boolean` comes as bits
from a bit offset, as Arrow keeps it. A `Dictionary` keeps a list of
values once and an index into it a slot; its values are converted only
where a slot asked refers to them, so that a value no row uses never
fails, and the index of a `NULL` slot or of a slot not asked is not
read, since it may point anywhere. A string's bytes are checked in
place.

A **view array**, Arrow's `Utf8View` or `BinaryView`, comes as the bytes
of its views, 16 a slot in Arrow's layout, and the slices of its
buffers. A value of up to 12 bytes lies in its view, so that its bytes
are a slice of the views: the crate has no unsafe code, and a `u128`
cannot be lent as bytes without it. A long view is checked against its
buffers before its bytes are read, and one that does not hold is a
broken shape. The view of a `NULL` slot is not read, since Arrow lets it
hold anything. A dictionary of views is refused: no caller gives one,
and its values' shape would need rules of its own.

A write packs the slots asked, one Arrow value a slot asked, in their
order: the Arrow array it makes holds only the rows the caller chose. A
write of every slot, as a `COPY` makes, asks for every slot of the
column: a mask of all ones, the bits past the column's last slot clear,
as [`WritePlan::write`](../../../crates/datum-arrow/src/write.rs) wants
them.

Refused: a list of slot numbers for the slots asked, which makes every
loop a gather, slow on both machines; a copy of a dictionary expanded
into a plain column, which converts and checks values no row uses; and
views turned into offsets and bytes by the caller, the copy that views
exist to spare.

## Loops a compiler vectorizes

The conversions of values passed by value are elementwise arithmetic
with a check of range: an epoch shifted, a unit multiplied, an integer
widened. Such a loop runs on the vector units of AArch64 and x86-64
without intrinsics when it has no branch per value, so the loops are
written that way.

- A loop takes the 64 slots of a word of the mask at a time.
- A plain array of integers, floating-point numbers, days, times or
  booleans converts every slot of a word, asked or not, into the
  output's places, so that the loop is dense. It converts the words
  from the first of the mask that asks a slot to the last, found once
  a call; the words before and after keep their places, and their words
  of the mask of `NULL` slots are 0. A caller that asks a few rows of a
  long column, as the Parquet node asks a batch of 64 rows of a window
  of 1 024, then converts their one or two words, not the column. A
  branch for each word that asks no slot, refused, would put a branch
  on the caller's data in the dense loop. A dictionary and bytes of
  fixed size are outside this set: they write the places of the slots
  asked alone. A slot not asked that is not `NULL` gets its true
  representation where its value converts, so a place the caller filled
  before keeps its value; any other place gets a value of no meaning.
  The documentation of
  [`ReadPlan::read`](../../../crates/datum-arrow/src/read.rs) says so.
- A check folds its result over the 64 slots into one word, masked by
  the slots asked and not `NULL`; the first failing slot is the first
  set bit of that word.

Strings and bytes are checked and given for the slots asked alone: the
check of a string is a loop over its own bytes, and the standard
library's check of UTF-8 already runs by words.

The machine code of these loops was read on both machines, from
`cargo rustc --release -p datum-arrow --lib -- --emit asm`. On AArch64
the loops of days and of integers are vectors of NEON; the loop of a
64-bit time keeps one scalar multiplication a value, since NEON has no
64-bit multiplication; no loop has a branch per value. On x86-64 the
default build, for the first processors of the line, keeps the loops
scalar but without a branch per value, since that set lacks the vector
comparison of 64-bit integers; built for AVX2 the same loops are
vectors of 256 bits. The crate keeps the default build and chooses no
code at run time.

Refused:

- **A crate of its own for vector code.** The conversions have one
  owner, this crate; a crate of vector code that shifted epochs or
  checked UTF-8 would be a second. Tessera's kernels, which hold its
  own vector code, cannot be a dependency of a crate free of Tessera.
- **Explicit intrinsics now.** They come only where the machine code
  and a measurement show the compiler's loop falls short, each behind
  the code of one value as its reference.
- **`simdutf8` for the check of UTF-8.** The standard library's check
  is fast already; the crate is a candidate once a measurement shows
  the check costs.
- **A choice of code at run time on x86-64.** The scalar loops have no
  branch per value; a second build for AVX2 waits for a measurement
  that asks for it.

## The bounds of a comparison

A reader that checks a clause such as `EventTime >= $1` on a file's
seconds turns the constant once into a
[`Test`](../../../crates/datum-arrow/src/bounds.rs) of the stored
integers, with
[`ReadPlan::test`](../../../crates/datum-arrow/src/bounds.rs): the
comparison with a bound in the stored unit, rounded up or down as the
operator needs, or "every value", or "no value". For example, `= 12.5 s`
passes no whole second, and `>= 12.5 s` is `>= 13` in seconds. The test
is exact over the whole stored domain, as if every stored value were
counted in microseconds since 2000 in 128 bits, so that a stored value
outside PostgreSQL's range compares by its true value, without an error,
and only a row that passes is converted and may fail. The infinities are
above and below every stored value. A plan of integers takes a constant
of any integer type, as PostgreSQL's B-tree family of integers compares
a `smallint` with a `bigint`.

The constant is divided by the stored unit in 64 bits, not 128. Every
epoch's shift is a whole number of its stored unit: 946 684 800 000 000
microseconds, the shift of a time, is a whole number of milliseconds
and of seconds. So floor((c + shift) / unit) is floor(c / unit) plus
the shift in stored units, a division in 64 bits whose result is exact.
`Count::time` asserts the shift whole, and a proof checks it of every
count the crate makes.

A test also answers for a closed range of stored values, such as the
least and the greatest values of a part of a file, with
[`Test::range`](../../../crates/datum-arrow/src/bounds.rs): no value
passes, some may, or every value does. The answer is of the values that
are not `NULL`, and a range whose least value is above its greatest, as
the statistics of a damaged file may give, gets "some may", which rules
nothing out. [`Test::apply`](../../../crates/datum-arrow/src/bounds.rs)
gives the slots of a column that pass, a word of 64 slots at a time.

The bounds are for integers, days and times alone. A floating-point
number orders `NaN` as PostgreSQL does not, and a string by its
collation, which the crate does not know.

## Proofs and tests

The conversions of one value are small and closed, which suits Kani, a
checker that proves a property of a Rust function for every input
rather than for a sample. Its proofs, in `proofs.rs`, are built only
under Kani and run in a row of CI's Kani job, every cover reached:

- a day or a time read is accepted exactly when its exact count in 128
  bits is in PostgreSQL's range, and converted to it, for days and each
  unit of time;
- a date or a time written is accepted exactly when PostgreSQL and
  Arrow both hold it, and read back to the same value;
- the test of a bound against the exact comparison in 128 bits, for
  every stored value and every finite constant, for integers, days and
  microseconds; past the division of the constant by its unit, for
  every unit, and an equality's test there one bound or none, and none
  for `NULL` or an infinity; and the shift of every count a whole number
  of its unit;
- the answer for a range exact, for every least and greatest value;
- for plans of integers, of days in 16 bits, of 64-bit integers and of
  seconds, a failure exactly where the value is out of range and only
  where the plan names it;
- a string of up to four bytes read through a plan of checked bytes
  into `text`, refused exactly where it is not UTF-8 or holds a zero
  byte; and its characters counted as the standard library decodes
  them. The check of UTF-8 itself is the standard library's, which the
  proof calls on both sides: it shows the plan's checks wired right,
  not UTF-8.

The bounds of seconds and milliseconds divide a symbolic 64-bit
constant by a thousand or a million, which takes Kani past the ten
minutes of a harness, tried with the comparison fixed, with another
solver and as the floor property alone. So the computation of a bound
is split in two. First the constant is divided by the unit, as Euclid
divides: `constant = quotient * scale + rest`, with
`0 <= rest < scale`. That step is the standard library's. Then the
bound is taken from the quotient, whether the rest is zero, and the
shift in stored units. Kani proves the second step for every quotient,
shift and stored value, against the sign of the exact difference. The
difference is `units * scale - rest`, `units` being the stored value
less the shift and the quotient. Its sign is that of `units` where
`units` is not zero, and that of `-rest` where it is. The proof thus
holds no multiplication and no division, and finishes in seconds. The
property tests compare the two steps together with the exact count,
over random constants and the edges of every range.

Kani's cost grows with the length of a loop it unrolls, and it knows
few vector intrinsics, so it proves the code of one value. The loops
over a batch are compared with that code by property tests over
columns of every bit pattern, random `NULL` slots, bit offsets and
slots asked. A property test makes plans from every pair of known types
with every option and calls each over slots of every bit pattern: no
call panics, and each fails only as its plan names. Arrow's own crate
`arrow-array` builds arrays the crate reads and checks the arrays it
writes. The mutants of the crate, small changes of its code, are all
caught by its tests or named in `.cargo/mutants.toml` as changes that
give the same answers.

## What we decided not to do

- **`numeric` and `Decimal128`.** PostgreSQL's `numeric` is a sequence
  of base-10000 digits; its conversion is a plan of its own, both ways.
- **Nanoseconds now.** PostgreSQL holds microseconds, and a plan never
  drops digits: a plan of nanoseconds would name a new failure, a value
  with digits below a microsecond, and come when a reader needs it.
- **`interval`, `uuid`, times of day, nested types and unsigned 64-bit
  integers.** Each is a type of its own on one side or the other, added
  to the closed lists when a reader needs it.
- **A C API.** A reader reaches the crate from Rust; a C API waits for a
  caller in C, as entry points over the plans with their statuses.
- **Reading files.** The crate converts values from arrays a caller
  owns; reading and writing files is Apache Arrow's.

## Files

- **`crates/datum-arrow/Cargo.toml`**,
  **`crates/datum-arrow/README.md`**: the crate, its one dependency,
  and its front page with examples.
- **`crates/datum-arrow/src/lib.rs`**: the crate's root, its errors'
  overview and its exports.
- **`crates/datum-arrow/src/pg.rs`**: PostgreSQL's types, their object
  identifiers and type modifiers.
- **`crates/datum-arrow/src/arrow.rs`**: the Arrow types the crate
  takes.
- **`crates/datum-arrow/src/column.rs`**: Arrow's buffers borrowed,
  bitmaps, dictionaries' indexes and masks.
- **`crates/datum-arrow/src/calendar.rs`**: the epochs, the ranges and
  the counts of days and units of time.
- **`crates/datum-arrow/src/read.rs`**: read plans and the conversion of
  a column's slots.
- **`crates/datum-arrow/src/write.rs`**: write plans and the conversion
  of representations into Arrow's buffers.
- **`crates/datum-arrow/src/bounds.rs`**: the tests of stored values.
- **`crates/datum-arrow/src/error.rs`**,
  **`crates/datum-arrow/src/failures.rs`**: the errors with their
  SQLSTATEs, and the sets of failures a plan names.
- **`crates/datum-arrow/src/proofs.rs`**: the proofs by Kani, built only
  for Kani.

## Tests

- **`crates/datum-arrow/src/read/tests.rs`**: the pairs accepted and
  refused in order, the edges of each unit, strings checked, numbers
  widened, slots asked, `NULL` slots and dictionaries, every broken
  shape, and the property tests of the loops against the code of one
  value and of every input against a panic.
- **`crates/datum-arrow/src/write/tests.rs`**: the types written, the
  values, infinities and last times, the size of one call, broken
  shapes, the round trip of every type, and Arrow's own arrays.
- **`crates/datum-arrow/src/bounds/tests.rs`**: the bounds and the
  ranges against the exact count in 128 bits, and a test applied to
  columns of every width.
- The tests beside each module of `pg.rs`, `arrow.rs`, `column.rs`,
  `calendar.rs`, `error.rs` and `failures.rs`: their types, edges and
  codes.
- CI's job of the crate alone: its tests, its documentation with every
  warning an error and its examples, its dependencies, a dry run of
  publishing, the tests of the packed crate, and its public API against
  its snapshot; and its row of the Kani job.
