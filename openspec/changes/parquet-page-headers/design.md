# Design

## Context

The `parquet` crate reads a column chunk's pages through the reader's
shared file, a `ChunkReader`. It reads a page's data by `get_bytes`,
one read of its length. It reads a page's header by `get_read`, a
reader of the file from the header's offset, wrapped in its own
`TrackedRead`, which counts the bytes it hands out: that count is the
header's length, and the page's data starts after it. The crate's
Thrift reader reads a header's field markers, varints and numbers a
byte at a time; the bytes of a binary statistic it reads as one read
of their length. The crate makes a reader from an offset for each
header it reads, peeks or skips, and drops it after.

The shared file's reader from an offset, `At`, calls `pread` for each
read. A header of a few dozen bytes thus costs as many system calls.
The crate's own `ChunkReader` for a `File` wraps that reader in a
`BufReader`.

A profile of ClickBench's Q8, taken with the macOS tool `sample`, which
records the running function once a millisecond, counts 761 samples in
`pread` of 2 831: 680 under `At::read`, the headers, and some 80 under
`get_bytes` and elsewhere. The headers' system calls are thus 24 per
cent of the query.

The shared file counts its reads, `At`'s and `get_bytes`'s. The count
tells whether a window's reads reached the file since the last check of
the file, not how many system calls they made: the column reader checks
the file after the windows whose reads reached it.

A copy of the shared file made for a Snappy page reader keeps the bytes
its readers from an offset read, which are a header's. The page reader
parses the page's decompressed size from the header's first fields, at
most 12 bytes, and checks the page's decompressed bytes against it. It
takes the bytes kept after it reads a page, and clears them after it
skips one; a peek keeps them for the read that follows.

## Goals / Non-Goals

**Goals:**

- One `pread` for a header of up to a kilobyte, where it took one for
  each field marker, varint and number; the same bytes, pages and rows.
- The check of a page's size against its header as it is, with one
  lock and 12 bytes kept a header, where it took a lock and a copy for
  each read.

**Non-Goals:**

- The page indexes of a file, which would let the crate find pages
  without reading their headers: `hits.parquet` has none.
- The conversion of a selection into the crate's runs, and the trailing
  skip's cost: the entry `parquet-pages-cost`.

## Decisions

The reads of a header after the change:

```
 the crate's page reader
   │ get_read(the header's offset)
   ▼
 TrackedRead ─── counts the bytes handed out: the header's length
   │ a byte at a time; a statistic's bytes in one read
   ▼
 BufReader, 1 KiB ─── hands out bytes from its buffer; a read of
   │                  1 KiB or more passes it by
   │ one read a kilobyte
   ▼
 At ─── pread from the file; for a Snappy chunk keeps the first
        12 bytes from the header's offset, for the check of the
        page's size

 the page's data: get_bytes(the offset after the header, its length)
```

### A `BufReader` of 1 KiB over `At`

- **Was:** `get_read` gives `At`, whose every read is a `pread` of the
  bytes asked.
- **Will be:** `get_read` gives `BufReader::with_capacity(1024, At)`.
  The buffer's first read is one `pread` of a kilobyte from the
  header's offset. The Thrift reader's small reads take their bytes
  from it; a read past it fills it again, and a read of a kilobyte or
  more passes it by. A read is given at most the bytes it asks, so
  `TrackedRead` counts a header's length right.
- **Why the standard buffer:** it is what the crate's own reader of a
  `File` does, and it is the whole change: no buffer of the reader's own
  to write and test.
- **Why 1 KiB:** a header without statistics is some 20 to 60 bytes;
  one with the least and greatest values of a string column holds them
  too, and writers cut those to a few dozen or hundred bytes. A larger
  buffer reads more of each page's data, which `get_bytes` reads again,
  for every page skipped too, and allocates and zeroes more for each
  header. The size is checked by one count of Q8 and Q28 at 256 bytes
  and 8 KiB beside it.
- **Refused: the reader parses the headers itself,** from bytes it reads
  by `get_bytes`: a parser of Thrift's compact form for the whole
  header, reads of a fixed length clamped at the file's end, and the
  crate's page reader no longer reads them. Without a page index the
  crate's page reader has no other way to find its pages.

### The header kept, 12 bytes of it

- **Was:** `At` copies every byte it reads into the shared copy's list,
  under its lock.
- **Will be:** `At` copies only the bytes it reads within 12 bytes of
  the offset it starts from, which `header_size` reads at most: one
  lock at the buffer's first fill, none after. The page reader takes
  and clears the kept bytes as before.

### The count of reads

The count stays the count of `At`'s reads and `get_bytes`'s calls. A
header read is one read of `At` where it was a read for each field;
a window whose pages' headers were read still counts, and the column
reader checks the file after it as before.

## Measurement plan

On the M5 Pro, `hits.parquet` in the page cache: `cargo clickbench run
--counters --tries 5 --mode on --workers 0`, one query a run,
PostgreSQL 19 with `pg_pmu`, the base, the commit the work starts from,
and the end in turns, twice. Cycles decide, by the maintainer's rule;
instructions and pg_duckdb's cycles at one thread stand beside them.
pg_pmu counts the kernel too on macOS, so the system calls are in both.

Queries: Q8, which groups the rows of `AdvEngineID <> 0` by
`AdvEngineID`, and Q2, which counts them; Q11, which skips the pages of
`MobilePhoneModel`; Q28 and Q26, whose pages are read whole; Q3, which
reads numbers; Q1, which reads no page and is the control.

Hypotheses, written before the runs:

- Q8: 15 to 23 per cent fewer cycles, its headers' system calls, of
  24 per cent; fewer instructions too;
- Q2: as Q8, within 5 points; not profiled;
- Q11: 5 to 12 per cent fewer cycles, of `pread`'s 12 per cent;
- Q28, Q26, Q3, Q1: within 2 per cent of cycles.

How a query is judged:

- A side's spread is the difference of its two turns. A change of
  cycles smaller than the larger spread of the two sides is no change.
- When Q1, the control, or a side spreads past 2 per cent, that query
  is counted once more; still past it, it is reported unsettled, not
  passed or failed.
- The change is kept when Q8 loses at least 12 per cent of its cycles,
  Q11 at least 4, and no query counted gains more than 2.
- A confirmed excess is profiled and searched as CONTRIBUTING.md says
  before the maintainer is asked.

The report is kept under `docs/benchmarks/`, as `docs/measuring.md`
says.

## Risks / Trade-offs

- [A header longer than the buffer] → read by more fills, or past the
  buffer for a long statistic; tests read and skip pages whose headers
  pass a kilobyte.
- [A page's data read twice, the buffer's read-ahead and `get_bytes`]
  → under a kilobyte a page, against pages of tens or hundreds of
  kilobytes.
- [A buffer allocated and zeroed for each header] → a kilobyte,
  against some 20 system calls.
- [The kept bytes lost between a peek and a read] → the size would go
  unchecked, silently; tests peek, skip and read in every order and
  give a wrong size after each.

## Decisions of the maintainer

- Cycles decide the counts; instructions help but come second, above
  all where the work is input and output.

## Files

- `crates/tessera-parquet/src/reader.rs`: the shared file's reader from
  an offset and its kept bytes.
- `crates/tessera-parquet/src/pages.rs`: the comments on the kept
  bytes.
- `crates/tessera-parquet/src/reader/tests.rs`,
  `crates/tessera-parquet/src/pages/tests.rs`: the tests below.
- `openspec/specs/parquet-fdw/design.md`, at the close.

## Tests

- A header read reaches the file once, by the count of reads.
- Pages whose headers pass a kilobyte, of strings with statistics of
  some 600 and 3 000 bytes written into the headers, Snappy and not,
  read and skipped as the crate reads them over a `std::fs::File`.
- A page's wrong size is an error after a read, after a peek and a
  read, after peeks and a read, after a peek, a skip and a read, and
  after a skip and a read, over adjacent Snappy pages of other sizes.
- The last page of a file, whose buffer's fill is short, reads right; a
  file cut in a header or in a page's data is an error as before.
- The tests of the count of reads as they were.
