## Context

See proposal.md, "Why", for the numbers. What the code does today:

- The reader asks the `parquet` crate for a column through a view of
  the file's footer and an Arrow schema of its own. A column of bytes
  has two views:
  - a chunk whose every data page is encoded through its dictionary
    is read as a dictionary of `LargeBinary` with `Int32` indexes;
  - any other chunk, plain or one that falls back from its dictionary
    to plain pages, is read as a plain `LargeBinary`.
- To make a `LargeBinary`, the crate copies each value's bytes out of
  the decompressed page into one buffer and writes its offset. Rows
  read through the dictionary of a chunk that falls back are copied
  out of the dictionary the same way, one row at a time.
- The reader reads arrays of a fixed count of rows and gives them
  window by window. A window that spans two arrays joins them, which
  copies the bytes again.
- `datum-arrow` takes an array as plain slices: offsets and bytes for
  strings. It depends on `arrow-schema` alone, so it cannot name the
  crate's types of arrays and buffers.
- The bytes form of a plain window gives, for each row, a pointer into
  the window's buffer of bytes and a length. Before it gives them, the
  reader checks the whole window in one pass: the bytes from the first
  offset to the last are UTF-8 without a zero byte, and no value
  starts inside a character. A window that fails is read row by row
  through the column's plan, which fails at the plan's row.
- The clauses the source checks read a plain window's strings by
  their offsets; through a dictionary, once a value.

## Goals / Non-Goals

Goals:

- A value read from a chunk that is not encoded through its dictionary
  in every page is not copied: its bytes stay in the decompressed page
  or in the dictionary page, or, up to 12 bytes, in its view.
- Every result, error and row of an error stays as it is.
- The check of a value keeps its one owner, the column's read plan, and
  the bytes form keeps a cheaper check that the plan's own tests judge.

Non-goals: see proposal.md, "Not in this change"; and no change of the
C API, of a setting, or of how a chunk encoded through its dictionary
in every page is read.

## The whole

```
 a chunk of a string column
 ──────────────────────────
 dictionary page ─┐
 data pages:      │   the parquet crate, asked BinaryView
  through it ─────┼──▶ views of 16 bytes, one a row
  plain ──────────┘        │
                           │ a value of up to 12 bytes inside its view,
                           │ a longer one an index of a buffer and an
                           │ offset; the buffers are the decompressed
                           │ dictionary page and data pages
                           ▼
 a window of rows ── the bytes form: a pointer into a page or into
 (a slice of views     the view, and a length, a row
  and the buffers) ─── the clauses: each value's bytes in place
                   ─── datum-arrow: the values a consumer asks for,
                       checked and made as from LargeBinary
```

A chunk whose every data page goes through its dictionary keeps its
dictionary array: its values are copied once a chunk, and its indexes
number the rows for TessAgg and for the clauses' memo of answers.

## Decisions

### Views for a chunk that is not a dictionary in every page

Was: such a chunk is read as `LargeBinary`, a copy of every value.
Will be: it is read as `BinaryView`. The crate's reader of views keeps
each decompressed page as a buffer of the array without a copy, and
points a long value's view at its place there. Through the dictionary
it copies the dictionary's view, 16 bytes, not the value. Joining two
arrays copies their views and shares their buffers.

Refused:

- Views for every chunk. The crate cannot give a dictionary whose
  values are views, so a chunk that is a dictionary in every page
  would lose its indexes. TessAgg numbers such a chunk's rows once an
  index, and the clauses answer once a value.
- The reader's own decoding of the pages of `BYTE_ARRAY`, as the
  roadmap's entry first said. It would give the same places in the
  pages. But it needs the crate's decoders, which sit behind the
  feature `experimental`, and its own checks of damaged pages and of
  the selection of rows. The views give the same places through the
  crate's public reader.
- `Utf8View`, with the crate's own check of UTF-8. The crate checks a
  whole page at once and fails the read with an error of its own, not
  `22021` at the plan's row. It would also fail on a value that no row
  asked for, which a clause rejects. It does not check zero bytes.

### The input of a view array in `datum-arrow`

Was: `Values` holds a string column as offsets and bytes.
Will be: a new variant holds a view array as the bytes of its views,
16 a view in Arrow's layout, and its buffers, a slice of byte slices.
The crate's documentation draws the view's layout. The views are bytes,
not `u128`s, so that a short value is a slice of its view: the crate
has no unsafe code, and a `u128` cannot be borrowed as bytes without
it. A view of a `NULL` slot is not read, since Arrow lets it hold
anything. A long view's index of a buffer, its offset and its
length are checked against the buffers given before its bytes are
read; one that does not hold is a broken shape, `XX000`. `Utf8View`
and `BinaryView` take the pairs and the failures of `LargeUtf8` and
`LargeBinary`.

The reader makes the slice of byte slices for each read of a window,
from the array's buffers, into a place its caller gives, so that the
input lives as long as that place does. A dictionary's values stay
`LargeBinary`, so the reader keeps a plan of their own for the check of
a dictionary once.

Refused:

- The buffers as the crate's own type: `datum-arrow` may depend on
  `arrow-schema` alone, by its contract.
- Views turned into offsets and bytes before `datum-arrow` reads them:
  that is the copy this change removes.

### The bytes form and its check over views

Was: a plain window's rows point into its one buffer of bytes, and one
pass checks the bytes from the first offset to the last.
Will be: a row's pointer is into the buffer its view names, or into
the view itself for a value of up to 12 bytes. The array that the
window holds keeps both alive. The reader takes a value through the
crate's own accessor of a view array, as it takes a dictionary's
values by the offsets of a valid array. The window is checked once,
value by value, over its rows that are not `NULL`, each value in one
pass by words of 8 bytes: a zero byte found by an exact mask, and a
byte at or above 0x80 sending the value to the full check of UTF-8,
which a value of ASCII never takes. The check of a plain window in one
pass goes, with its proof: no reader's array is plain bytes by offsets
any more.

A window that passes gives every row asked its bytes unchecked again,
as today. A window that fails is read row by row through the plan,
which fails as it fails today.

Refused:

- One pass over the bytes of a run of values. The values no longer lie
  together: a plain page puts each value's 4-byte length before it,
  and a short value lies in its view. The crate checks UTF-8 across
  those lengths when each is below 128. A zero byte cannot be checked
  that way, since the lengths hold zero bytes. The run checked for
  UTF-8 in one pass, its zero bytes counted against those of the
  lengths, is the way to try if the check by words costs too much.
- A search for a zero byte and a check of UTF-8 of their own, a call
  each a value. Counted: the two calls took 21 per cent of Q28's
  samples, and the copies saved went to them; the pass by words keeps
  the saving.
- Checking each distinct value of a dictionary once in a chunk that
  falls back. The crate does not say which views came through the
  dictionary. Today these rows are checked once a row too.

### The clauses over views

Was: the clauses read a plain window's strings by their offsets.
Will be: they read each value's bytes through the crate's own accessor
of a view array, as they took a valid array's offsets: the parquet
crate builds each view from a length it checked against its page, and
a `NULL` slot's view as an empty one. The dictionary's path does not
change.

### Memory held

A window held by a batch holds the array it is a slice of, and with it
the decompressed pages its views point into, whole. Before, it held a
copy of its rows' bytes. The reader reads a fixed count of rows at a
time, so an array spans a few pages of one chunk. The rows a forward
reader keeps past a window stay a slice of the array they were read
in, not of the joined window: a slice of a view array keeps every
buffer of its array, and a join every buffer of its parts, so a slice
of the join would carry each page read before into every later window.
A column held through a batch therefore holds a few decompressed pages
at most. The
memory stays the reader's, freed with the window, under the contract's
rule "The reader's memory is the query's".

### Architecture

- `parquet-fdw`'s forecast names the reader's windows and its C
  entries of the bytes form for this kind of change, "not the node's
  batches, the clauses or the wrapper". The change touches the
  clauses. They read the strings of the reader's arrays, and the type
  of those arrays changes. This is a deviation over the boundary
  between the clauses and the reader's arrays. It goes into the
  roadmap's entry for simplifying the part, `parquet-reader-boundary`.
- `datum-arrow`'s forecast names "the sets of types and the plans'
  rules; not the representations" for views of strings. The change
  touches the sets and the rules, and also `Values`, the input of a
  column, which the forecast does not name. The forecast is corrected
  to name the input. The deviation goes into a new roadmap entry for
  simplifying `datum-arrow`.
- No new dependency. The workspace already builds the crate's reader
  of views.

### Decisions of the maintainer

- 2026-11-24: after the sort by bytes, look first at views of strings
  in the reader, then at a faster decompression of Snappy, in a change
  of its own.
- 2026-11-24: the proposal approved as it stands.

## Was and will be

- A chunk that falls back from its dictionary: was `LargeBinary`,
  every value copied; will be `BinaryView`, no value copied.
- A plain chunk: the same.
- A chunk that is a dictionary in every page: unchanged.
- `datum-arrow`: was `Utf8View` and `BinaryView` refused, `0A000`;
  will be read as `LargeUtf8` and `LargeBinary`. A dictionary of views
  stays refused: no caller gives one, and its values' shape would need
  rules of its own.
- The bytes form of a plain window: was a pointer into one buffer of
  copies, checked in one pass; will be a pointer into a page or a
  view, checked value by value.
- The clauses: was offsets; will be views.

## What we do not do

- The decompression of Snappy: 34 to 39 per cent of the profiles, the
  next change, which compares other implementations.
- Views of a chunk that is a dictionary in every page: the crate cannot
  give them, and the indexes are worth more there.
- `Utf8View` in the reader: the crate's check of UTF-8 does not fail as
  the plan must (see "Views for a chunk that is not a dictionary in
  every page").
- The reader's own decoding of pages.
- A second check of UTF-8 for the dictionary part of a chunk that falls
  back.

## Risks / Trade-offs

- [The check value by value costs more than one pass for short
  strings] → the count shows it. The first way to try is written under
  "The bytes form and its check over views".
- [A held window holds whole pages] → bounded by the few pages of one
  array of the reader; freed with the window, as today.
- [The crate's views break a shape] → the bytes form and the clauses
  take the crate's views as valid, as they took its offsets: the crate
  builds each view from a length it checked against its page.
  `datum-arrow`, which takes views from any caller, checks every long
  view against its buffers before it reads it, and refuses one that does
  not hold.
- [A consumer keeps a pointer of the form past its batch] → the rule
  of `string-bytes` forbids it, as today; a page outlives no batch that
  holds it.

## How the code is checked

- `datum-arrow`: views read against the same values from `LargeBinary`
  and `LargeUtf8` by a property test, every pair and failure, the
  broken shapes of views, and no panic on any input, which covers the
  new variant.
- `tessera-parquet`: the cheaper check over views against the plan's
  row by row, as today for offsets; a file whose chunks fall back from
  their dictionary read by values and by the bytes form; the clauses
  over views.
- `test/sql/parquet.sql`: the scenario "A chunk that leaves its
  dictionary", against a heap table.
- `make crosscheck`, whose Parquet twins hold strings.
- Miri on the reader's tests of the bytes form over views, as on its
  tests of the bytes form today; `datum-arrow` has no unsafe code.

## Measurement plan

On the M5 Pro, with `cargo clickbench run --counters --tries 5 --mode
on --workers 0`, one query a run, over `hits.parquet`, PostgreSQL 19
with `pg_pmu`. Instructions decide and cycles are reported beside
them, with pg_duckdb's cycles at one thread. Every query of ClickBench
is counted against the commit the work starts from. Q34 and Q35 spill
and depend on the page cache, so they are counted as a pair, the base
and the end back to back. The heap is not read by the change and is
not counted.

Hypotheses, written before the runs:

- Q28, `avg(length(URL))`: 10 to 16 per cent fewer instructions, the
  copies of its profile;
- Q26, `ORDER BY SearchPhrase LIMIT 10`: 6 to 12;
- Q13 and the other queries that read `SearchPhrase`, `URL`, `Title`
  or `Referer` in full: fewer;
- the queries that read no string: within 1 per cent.

The change is kept when Q28 loses at least 8 per cent, Q26 at least 5,
and no query counted gains more than 1 per cent. Otherwise the run's
report says which way failed and why, and the maintainer decides. The
report is kept under `docs/benchmarks/`, as `docs/measuring.md` says.
