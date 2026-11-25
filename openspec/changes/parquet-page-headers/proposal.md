## Why

The Parquet reader's reader of a file from an offset, `At`, calls
`pread` for each read the `parquet` crate asks of it, and the crate
parses a page's header by its Thrift reader mostly a byte at a time:
each field marker, varint and number of each header a page reader
reads, peeks or skips is a system call. The crate's own reader of a
`File` puts a `BufReader` there; the reader's shared file does not.

A profile of ClickBench's Q8 at the end of `parquet-snappy`, taken
with the macOS tool `sample`, which records the running function once a
millisecond, shows it: `pread` is 761 of 2 831 samples, the most of
any function, and 680 of them are under `At::read`, the headers' reads:
24 per cent of the query. Q8 groups the rows of `AdvEngineID <> 0` by
`AdvEngineID`, and the crate skips most pages of `AdvEngineID` by their
headers. A
profile of Q11 at the end of `parquet-string-views` gave `pread` 12 per
cent. Q8 runs in 3.3 times pg_duckdb's cycles at one thread, Q11 in
3.7.

The reader also keeps the bytes of each Snappy page's header to check
the page's size, by a lock and a copy for each of those reads: 0.8 per
cent of Q8's instructions.

This change takes up the roadmap's entry `parquet-page-headers`.

## What Changes

- The shared file gives the crate, for a read from an offset, a
  `BufReader` of 1 KiB over `At`, as the crate's own reader of a `File`
  does: the Thrift reader's reads of one byte take their bytes from the
  buffer, and `At` reads the file a kilobyte at a time. The buffer
  gives a read no more bytes than it asks, so the crate's count of a
  header's bytes is the same. The page's data, which the crate reads by
  `get_bytes`, is read as now.
- A reader of the file from an offset lives for one header in the
  crate's page reader, so the buffer never holds bytes of another read.
- The bytes of a header kept for the check of a page's size are its
  first 12, the most the page reader parses, kept at the buffer's first
  fill by one lock, where every read took a lock and a copy.
- The reader's count of reads counts each read of `At` and each call of
  `get_bytes`, as now: a header costs one where it cost one a field.
- Results, errors, EXPLAIN and the settings stay as they are.
- Not in this change: the conversion of a selection into the crate's
  runs and the cost of the trailing skip, which the entry
  `parquet-pages-cost` holds; page indexes.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

None: what SQL sees does not change, and `skip_specs` is set. The
design of `parquet-fdw` says how the file is read from an offset.

## Impact

- Crates: `tessera-parquet`, `src/reader.rs` (`SharedFile`'s reader
  from an offset and its kept bytes), the comments of `src/pages.rs`,
  and their tests.
- Public headers, settings, SQL suites: none.
- Contracts: the change touches the reader's input alone, which
  `parquet-fdw`'s forecast names for files in an object store; no item
  of the forecast names this change, and the pull request compares what
  it touched under Architecture.
- Roadmap: the entry `parquet-page-headers` leaves it at the close.
