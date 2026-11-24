## Why

The Parquet reader copies each string it reads. It asks the `parquet`
crate for a column of `BYTE_ARRAY` as an Arrow `LargeBinary` array, and
the crate copies every value's bytes out of the decompressed page into
the array's buffer, one row at a time. A chunk whose every data page is
encoded through its dictionary is read as a dictionary array instead,
and costs one copy of the dictionary. But most chunks of a column of
many values start through a dictionary and then fall back to plain
pages, once the dictionary has grown to its limit. Such a chunk is
read as a plain array, and each of its rows read through the
dictionary is copied too. In `hits.parquet`, 136 of the 226 chunks of
`SearchPhrase` and 217 of the 226 of `URL` mix the two.

Profiles taken with the macOS tool `sample` record which function runs,
once a millisecond. Two such profiles, at the end of the sort by bytes
of `text-keys-by-bytes`, show where these queries spend their time:

- Q26, `ORDER BY SearchPhrase LIMIT 10`, 2 443 samples. The copies are
  12 per cent of them, under the crate's `OffsetBuffer::try_push`,
  `extend_from_dictionary` and its decoding of the dictionary page,
  and under the reader's joining of arrays. The check of UTF-8 is 16
  per cent, the decompression of Snappy 34.
- Q28, `avg(length(URL))` by `CounterID`, 2 900 samples. The copies
  are 13 to 15 per cent, the decompression 39.

Against pg_duckdb with one thread of DuckDB, counted by `pg_pmu`,
Tessera runs Q26 in 2.7 times DuckDB's cycles and Q28 in 2.5 times.
DuckDB does not copy a string. A value of up to 12 bytes sits inside
its 16-byte record, and a longer one is a pointer into the
decompressed page or into the dictionary.

The crate already gives the same thing. Asked for a column as
`BinaryView`, it gives an array of views. A view is a 16-byte record
of a value's length and either its bytes, up to 12 of them, or the
place of its bytes in one of the array's buffers. The buffers are the
decompressed pages and the dictionary page themselves.

This change takes the roadmap's entry `parquet-string-pages`. Its goal
is the same: a value's bytes given where the decompressed page holds
them. It gets there through the crate's views, not through decoding
of the reader's own.

## What Changes

- `datum-arrow` reads the Arrow types `BinaryView` and `Utf8View`
  under the rules of `Binary` and `Utf8`: into `bytea`, and, checked or
  trusted, into `text` and `varchar`. Its input gains a variant for
  view arrays: the views and the array's buffers. A view that points
  past its buffers is refused as a call of a broken shape.
- The Parquet reader asks for a chunk of bytes that is not encoded
  through its dictionary in every data page as `BinaryView`, not as
  `LargeBinary`. A chunk encoded through its dictionary in every data
  page is read as a dictionary array, as today.
- The bytes form of such a window points into the decompressed pages,
  or into the views for a value of up to 12 bytes. A window held by a
  batch holds the pages its values point into.
- The check of UTF-8 and zero bytes is made on each row's value. It is
  no longer one pass over the window's copied bytes, since those
  bytes no longer lie together.
- The clauses the source checks on strings read view arrays too.
- Results, errors and their rows, EXPLAIN and the settings stay as they
  are. A string that fails its column's check fails the query at the
  same row and with the same `SQLSTATE`.
- Not in this change: the decompression of Snappy, a later change of
  its own; chunks encoded through their dictionary in every data page;
  the reader's own decoding of pages.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

- `datum-arrow`: the Arrow types it knows and the read plans it makes
  gain `BinaryView` and `Utf8View`; its values read and its broken
  shapes gain view arrays.
- `parquet-fdw`: reading rows promises the same rows and errors for a
  chunk that falls back from its dictionary to plain pages, by every
  path.

## Impact

- Crates: `datum-arrow` (`src/arrow.rs`, `src/column.rs`,
  `src/read.rs`, its public API and `public-api.txt`) and
  `tessera-parquet` (`src/column.rs`, `src/reader.rs`, `src/c.rs`,
  `src/bytes.rs`, `src/clause.rs`). The workspace takes no new crate.
- Public headers: none. `TessBytesColumn` already gives a pointer and a
  length a row.
- Settings: none.
- SQL suites: `test/sql/parquet.sql` gains a file whose chunks fall
  back from their dictionary to plain pages.
- Contracts: the forecasts of `parquet-fdw` and `datum-arrow`. The
  change touches the clauses and the input of `datum-arrow`, which
  neither forecast names.
- Memory: a held window holds whole decompressed pages, not copies of
  its rows.
- Roadmap: the entry `parquet-string-pages` leaves it at the close.
