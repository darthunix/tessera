## Why

Tessera reads heap tables in batches and, since `source-scan`, any
relation a registered source reads; but no real source exists yet. The
maintainer chose ClickBench over its Parquet files as Tessera's next
proof, and the numbers say a heap table will not do:

- On a tenth of ClickBench's rows in a heap table, in memory (the run
  `clickbench-kh13CL` on the M5 Pro), the 43 queries took 21.4 s with
  Tessera, 40.5 s with the core and 53.4 s with pg_duckdb; but a column
  behind the text columns cost Tessera what it cost the core (0.97 of
  its time on column 41, 0.99 on column 105).
- On ClickBench's main machine, c6a.4xlarge with 32 GB, the heap table
  does not fit in memory: every heap entry takes about 258 s a query.
  The columnar entries over the same machine, pg_duckdb over Parquet and
  pg_mooncake, reach a hot geometric mean of about 0.65 s and
  0.78 s a query; both run the queries in DuckDB, with all 16 threads.

ClickBench's files are written by parquet-cpp 1.5.1: 105 columns,
Snappy, dictionary and plain encodings, about 500 000 rows a group of
rows (450 560 and 549 440 in `hits_0.parquet`). `EventTime` and the
other times are seconds since 1970 in a 64-bit integer, `EventDate` days
in an unsigned 16-bit one, and every string is bytes without a UTF-8
mark. Every chunk has its least and greatest values in the fields of
the column order its type gives (`EventTime` 1373832000 to 1373918399,
`CounterID` 17 to 38, `EventDate` 15901 alone in both groups), its count
of `NULL` values, and its statistics of encodings: 198 of the 210 chunks
are encoded through a dictionary in every data page, and twelve, of
`WatchID`, `Title`, `URL`, `Referer` and, in one group, four more, fall
back to plain pages; the dictionary pages are headed `PLAIN_DICTIONARY`,
as parquet-cpp writes them for the format's first version.

The 43 queries, by a rough classification:

- 17 read no string column: integer kernels and parallel reading decide
  them.
- 18 filter on a string, most by `SearchPhrase <> ''` or `URL <> ''`,
  which a value's length answers without building a value; 4 by
  `LIKE '%…%'` (Q21 to Q24), a search for bytes in bytes, and one
  computes `REGEXP_REPLACE` (Q29).
- 15 group by a string (`URL`, `SearchPhrase`, `Title`,
  `MobilePhoneModel` and others); Q34 and Q35 group all the rows by
  `URL`, where a PostgreSQL value built for each row before its hash
  would cost what reading the file saved.
- Q37 to Q43 filter by `EventDate BETWEEN …`; pg_duckdb's entry reads
  the times through a view of expressions such as
  `'epoch'::timestamp + (r['EventTime'] * interval '1 second')`.
- Q24 is `SELECT * … ORDER BY EventTime LIMIT 10` over the 105 columns.

pg_duckdb divides one file, `hits.parquet`, among its threads: a reader
that cannot divide a file's groups of rows among PostgreSQL's parallel
workers loses by a multiple whatever its kernels.

## What Changes

- A new module of this repository, the extension `tessera_parquet`: a
  foreign data wrapper `tessera_parquet` over Parquet files read in
  place, its source of the source registry and its batch node.
- A foreign table names its files, one or more, as paths or patterns;
  its columns map to the files' columns by name, with the types the spec
  lists, each through a read plan of `datum-arrow`
  (`openspec/specs/datum-arrow/`). A column option gives the unit of a
  time stored as a bare integer, so that seconds come up as `timestamp`
  and days as `date`, and another says whether bytes are trusted as
  UTF-8 or checked when they become a value.
- With Tessera off, or where the planner prefers it, the wrapper returns
  rows, as any wrapper does.
- With Tessera on, its source reads the table in batches: it reads a
  column only when a consumer asks for it, for the rows its clauses
  keep, and makes PostgreSQL values only for the rows the consumer asks
  for, so that a top-N sort reads its other columns for the rows it
  keeps; a count of rows needs no column at all.
- The source checks exactly the clauses it can on the file's stored
  values: comparisons of an integer column with an integer of any
  integer type and of a date or time column with a value of its own
  type, `IN` lists, `IS NULL`, a string's equality with a
  constant (`<> ''` by its length), and `LIKE` with a pattern of fixed
  parts and `%`, by a search of bytes. It skips a group of rows whose
  least and greatest values, `NULL` counts or dictionary no row could
  pass.
- Parallel plans divide a table's groups of rows among the participants
  through the plan's shared memory, within one file as across files.
- Estimates come from the files' metadata, and `ANALYZE` samples the
  rows for the column statistics; `EXPLAIN` shows the files, the groups
  of rows read and skipped, and the values decoded.
- Setting a table's files, or trusting a column's strings, takes a
  superuser or a role with the privileges of `pg_read_server_files`, as
  `file_fdw`'s file name does.
- A Rust crate of the workspace reads the files, over the `parquet`
  crate of Apache Arrow's Rust implementation, and converts values
  through `datum-arrow`.
- A measuring tool, `cargo clickbench`, runs ClickBench's queries over
  the files with Tessera on and off, and over pg_duckdb where it is
  installed, checks the answers against each other, and times them as
  ClickBench does.
- The roadmap: the entry `parquet-fdw` is taken; grouping by a string on
  the file's bytes, one PostgreSQL value a group instead of one a row,
  becomes the entry `text-keys-by-bytes`, after this change; and the
  ClickBench entry itself, `clickbench-entry`.

## Capabilities

### New Capabilities
- `parquet-fdw`: the foreign data wrapper over Parquet files, its types
  and options, its rows and its batches through its source, the clauses
  it checks, the groups of rows it skips, its parallel reading, its
  estimates and `EXPLAIN`.

### Modified Capabilities

None: the module is a source of `source-scan` and keeps its contract as
it stands.

## Impact

- New directories: `parquet/`, the module `tessera_parquet` with its
  control file and script; `crates/tessera-parquet`, the reader;
  `tools/tessera-clickbench` and `bench/clickbench/`, the measuring tool
  and ClickBench's queries.
- Dependencies: the crate `parquet` (Apache-2.0, 97 M downloads, 967
  crates depend on it, released 2026-11-17; Apache Arrow's `arrow-rs`
  holds 3 676 stars), with its features `arrow` and `snap` alone: they
  bring Arrow's array, buffer, data, schema, select and IPC crates,
  `base64` and `snap`, and the crate always needs `bytes`, `chrono`,
  `half`, `hashbrown`, `num-bigint`, `num-integer`, `num-traits`,
  `paste`, `seq-macro`, `thrift` and `twox-hash`; `datum-arrow`, a crate
  of the workspace; `memchr`, already in the workspace, for the search
  of bytes.
- Public headers: none. The module uses the public C API of Tessera
  alone and keeps the reader's entry points in a header of its own.
- Settings: none new. `tessera.enable` covers the source.
- Tests: a new suite `test/sql/parquet.sql` with a test module that
  writes its Parquet files; the installed set builds the module from
  copies against the installed headers and runtime library.
- CI: the module builds and its suite runs in the PostgreSQL jobs; the
  crate `tessera-parquet` joins the Rust jobs and the mutants of a pull
  request's changed lines.
- Roadmap: `parquet-fdw` taken, after `types-crate`; new entries
  `text-keys-by-bytes` and `clickbench-entry`.
