## Why

Tessera is about to read Parquet files (the entry `parquet-fdw`), and
later to write them, such as a `COPY` of a table into a Parquet file.
Both cross the same boundary: between PostgreSQL's types, which
Tessera's batches carry as `Datum` values, and the types of Apache
Arrow, the form in memory into which Apache Arrow's Rust implementation
reads Parquet files and from which it writes them. Nothing in the
repository says which PostgreSQL type a column of an Arrow type may
become, or the reverse, and the conversions hide traps a reader written
for one file would meet one by one:

- ClickBench's files keep times as bare integers, seconds since 1970 in
  an `INT64`, and dates as days in an unsigned 16-bit integer; their
  strings are bytes without a mark of UTF-8.
- PostgreSQL counts dates in days and times in microseconds since
  2000-01-01, Arrow since 1970-01-01, within limits that differ: a
  `timestamp` of PostgreSQL's last thirty years, up to 294276 AD, has no
  microseconds since 1970 in 64 bits, and PostgreSQL has `infinity`,
  which Arrow has not.
- A PostgreSQL `text` must be valid in the server's encoding and holds
  no zero byte; Arrow's strings may hold one, and Arrow's bytes need
  not be UTF-8 at all.
- A `varchar(n)` bounds its characters, which a byte count does not
  tell; a `timestamp(0)` rounds what a file keeps in microseconds; and
  no PostgreSQL value is larger than 1 GB, which an Arrow string may
  be.

A check per value of each such rule, decided anew for every value,
costs on every row; one forgotten loses data or lets an invalid value
in. The maintainer decided on 2026-11-21 that the boundary is a crate of
its own, built before the Parquet wrapper, with a plan of each column's
conversion checked once, before any value is converted.

## What Changes

- A new crate of the workspace, `datum-arrow`, free of Tessera and of
  PostgreSQL's own code, meant for use outside Tessera
  (`publish = true`), as `relhash` and `relspill` are; it depends on
  Apache Arrow's `arrow-schema` alone, and on `arrow-array` only in its
  tests, which compare its buffers with Arrow's own arrays.
- It names the PostgreSQL types it knows, by their object identifiers
  and type modifiers, and the Arrow types it takes.
- A **read plan**, made from an Arrow type, a PostgreSQL type and the
  options of the column (a bare integer read as days, seconds,
  milliseconds or microseconds, bytes checked as UTF-8 or trusted, the
  server's encoding), refuses a pair it
  cannot convert, and says every way one of its values may still fail.
- A **write plan**, made from a PostgreSQL type and the server's
  encoding, gives the Arrow type a column becomes and says the same of
  its values.
- The conversions of values, both ways, on PostgreSQL's
  representations, the forms its values have inside the server: dates,
  times, integers, floating-point numbers, booleans, strings and bytes,
  over a column's slots with their bitmap of `NULL` values, each failure
  with its SQLSTATE and its slot.
- The bounds of a comparison in a file's own unit, so that a reader can
  compare a column's stored integers with a PostgreSQL constant without
  converting any value.
- CI: the crate's tests, its Kani proofs, its mutants, and the snapshot
  of its public API, as for `relhash` and `relspill`.
- The roadmap: the new entry `types-crate`, taken; `parquet-fdw` depends
  on it.

## Capabilities

### New Capabilities
- `datum-arrow`: the crate of the boundary between PostgreSQL's types
  and Arrow's, its read and write plans, the conversions of values and
  the bounds of comparisons.

### Modified Capabilities

None.

## Impact

- New directory: `crates/datum-arrow`, with its rustdoc complete, as a
  crate meant for use outside Tessera must have it.
- Dependencies: `arrow-schema` (Apache-2.0), the schema part of the same
  Apache Arrow release the `parquet` crate will bring; `arrow-array` of
  that release, for the tests alone.
- Public headers, settings, SQL suites: none. The crate has no C API in
  this change; the Parquet wrapper reaches it from Rust.
- CI: the crate in the Rust jobs and a job of its own, "datum-arrow
  alone", as `relhash`'s, with the snapshot of its public API kept in
  the change's folder and moved to `openspec/specs/datum-arrow/` at the
  close.
- Roadmap: the entry `types-crate`, taken; `parquet-fdw` depends on it.
