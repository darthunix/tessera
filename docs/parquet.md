# Parquet files

The extension `tessera_parquet` reads Parquet files where they lie, as
foreign tables. A foreign table is a table whose rows a foreign data
wrapper reads from elsewhere; any PostgreSQL client can query it. With
Tessera's setting `tessera.enable` off, the wrapper returns rows, as
any wrapper does. With it on, its batch source reads the table in
batches of up to 64 rows through a node of its own, `TessParquetScan`:
it checks the common clauses on the file's stored values, skips the
groups of rows in which no row can pass, and makes a PostgreSQL value
only for the rows and columns a query asks for.

The promises are the capability `parquet-fdw`'s spec, with every rule
and every error's SQLSTATE; its design says how the part is built
(`openspec/specs/parquet-fdw/`). This guide shows how to use the
wrapper. ClickBench's table over its files,
[bench/clickbench/create.sql](../bench/clickbench/create.sql), is a
complete example.

A few words used below. A Parquet file keeps a table by columns. Its
rows come in **groups of rows**; in a group each column is a **column
chunk**, stored as **pages**, each compressed apart. A chunk may be
**encoded through a dictionary**: its distinct values are stored once,
in a dictionary page, and each row as an index into them. The file's
**footer**, its metadata at its end, gives every group's rows and every
chunk's place and size, and often its least and greatest values, its
count of `NULL` values and the encodings of its pages. A column's
**physical type** is how the file stores a value (`INT32`, `INT64`,
`BYTE_ARRAY` and so on); its **mark**, the format's logical or
converted type, says what the value means: a 16-bit integer, a date, a
string.

## Installing

`make && make install` builds and installs the module with Tessera's
others; it needs no line in `shared_preload_libraries` of its own. In
each database:

```sql
CREATE EXTENSION tessera_parquet CASCADE;   -- needs the extension tessera
CREATE SERVER parquet FOREIGN DATA WRAPPER tessera_parquet;
```

A server takes no options, and no user mapping is needed. The batches
need Tessera's bridge loaded, as the [README](../README.md) preloads
it; without the bridge the wrapper still returns its rows.

## A table over files

```sql
CREATE FOREIGN TABLE hits (
    watchid      bigint     OPTIONS (column 'WatchID'),
    counterid    integer    OPTIONS (column 'CounterID'),
    eventtime    timestamp  OPTIONS (column 'EventTime', unit 'seconds'),
    eventdate    date       OPTIONS (column 'EventDate', unit 'days'),
    url          text       OPTIONS (column 'URL'),
    searchphrase text       OPTIONS (column 'SearchPhrase')
) SERVER parquet
  OPTIONS (files '/data/hits.parquet, /data/more/hits_*.parquet');
```

- **`files`**, required: a comma-separated list of absolute paths. Each
  is a file or a pattern of the shell, with `*`, `?` and `[…]`. The
  list is read in its order, and a pattern's matches in the order of
  the bytes of their names. Spaces around a comma are dropped, and a
  path cannot hold a comma.
- The patterns are expanded when a query is planned and again when its
  scan begins. A file added or removed is seen by the next query, also
  by a prepared statement planned before.
- Setting or changing `files` takes a superuser or a role with the
  privileges of `pg_read_server_files`: the server reads the files with
  its own rights, as `file_fdw` does.
- A column reads the file's column of its own name, or of the name its
  option `column` gives, compared exactly, case and all. ClickBench's
  files name their columns in mixed case, so the table above names
  each.
- Every file must hold each column a query reads. A column no query
  reads is not checked, whatever its type in the file.
- The columns are declared by hand: the wrapper has no `IMPORT FOREIGN
  SCHEMA`. A table of the wrapper is read-only.

## Types

The wrapper reads a file by its physical types and marks alone. It
ignores a schema of Apache Arrow that a writer may keep in the file's
metadata. Where a file gives both a logical type and the older
converted type, the logical type decides. The requirement "The columns
and their types" of [the spec](../openspec/specs/parquet-fdw/spec.md)
lists every pair of a file's type and a
declared type the wrapper reads; in short:

- booleans to `boolean`, and integers to `smallint`, `integer` or
  `bigint` as wide as their mark or wider;
- dates to `date`, and times in milliseconds or microseconds to
  `timestamp` or `timestamptz` as their mark says whether they are
  adjusted to UTC; bare integers that count days or a unit of time to
  either by the option `unit` below;
- floating-point numbers to `real` or `double precision`;
- strings, and unmarked bytes, to `text`, `varchar` or `bytea`.

A query that reads a column the file lacks, a file column of another
type, or a pair the list does not give, fails when it is planned and
again when its scan begins. So does a chunk of a column it reads that
is compressed other than by Snappy or not at all. A value that its
declared type cannot hold, such as a time past the range of
`timestamp`, fails when it is made into a PostgreSQL value. The spec's
requirement "The columns and their types" gives the order of these
checks and their SQLSTATEs.

### Times kept as integers: `unit`

Some writers keep a time as a bare integer and say nothing of what it
counts. ClickBench keeps `EventTime` as seconds in an `INT64` and
`EventDate` as days in an unsigned 16-bit `INT32`. The option `unit`
says what such a column counts, from 1970-01-01 00:00:00:

- `'days'`, on an `INT32` without a mark of a date or a time: the
  column is a `date`;
- `'seconds'`, `'milliseconds'` or `'microseconds'`, on an `INT64`
  without a mark of a time: the column is a `timestamp` or a
  `timestamptz`.

A time mapped so is a plain column of its type. A clause such as
`eventdate BETWEEN '2013-07-01' AND '2013-07-31'` is then a comparison
of a column with constants, which the source checks on the stored
integers (below). Over a view that converts the integers by an
expression, every row would go through PostgreSQL's evaluation.

### Strings kept as bytes: `utf8`

Many files keep their strings as bytes without a mark, and nothing
says they are UTF-8. The wrapper reads every `BYTE_ARRAY` as bytes,
marked or not. A `text` or `varchar` column needs a database of the
encoding `UTF8`; a `bytea` column takes the bytes as they are. The
option `utf8` of a `text` or `varchar` column says what is checked:

- `'check'`, the default: each value is checked as UTF-8 without a zero
  byte, and as no longer than a `varchar`'s length, when it is made into
  a PostgreSQL value. Only the values made are checked. Rows that a
  clause of the source rejects, columns no query reads, and rows that a
  top-N sort passes over cost no check and raise no error.
- `'trust'`: nothing is checked. Whoever made the table promises that
  the strings are UTF-8 and hold no zero byte. It takes the privileges
  that `files` takes, only `text` and `varchar` without a length take
  it, and the column must name its file column by the option `column`,
  so that a rename cannot move the trust to another file column.

The wrapper's rows make every value of every column a query reads, the
source's batches only the values a consumer asks for. A string that
fails its check in a row that a clause rejects therefore stops the
query with Tessera off but not with it on. When no value fails, both
give the same rows.

## Rows and batches

With `tessera.enable` off, the plan reads the table through the
wrapper's `Foreign Scan`, row by row, and PostgreSQL checks the clauses
on each row. With it on, the wrapper's source offers Tessera's planner
a path of `TessParquetScan` beside the wrapper's own, and the planner
takes the cheaper. Tessera's batch parents, such as `TessAgg`,
`TessSort` and `TessHashJoin`, read the node's batches directly, with
no `TessPack`. A query whose targets or clauses read a whole row of the
table, such as `SELECT t FROM t`, or a system column, such as
`tableoid`, is read by the wrapper's scan.

The node makes a PostgreSQL value only when a consumer asks a batch for
its column, and only for the rows asked:

- a top-N sort over `SELECT *` makes the sort's column for every row,
  and each other column only for the rows the sort keeps;
- a `count(*)` without a clause reads no data page: the footers give
  the rows;
- a clause the source checks reads its column's stored values, makes
  no value, and leaves the other columns unread for the rows it
  rejects.

The node's batches also offer their strings as the bytes the pages
hold, without a PostgreSQL value a row, to a consumer that reads them
so, such as a grouping by a string; the capability `string-bytes`
describes that form.

## What the source checks

The source checks a clause on the file's stored values, before any
value is made, when the clause is one of these:

- `=`, `<`, `<=`, `>`, `>=` or `<>` of a column of an integer type with
  an integer of any integer type, of a `date` column with a `date`, of
  a `timestamp` column with a `timestamp`, or of a `timestamptz` column
  with a `timestamptz`;
- such a column `IN` a list, or `= ANY` an array, of such values;
- `IS NULL` and `IS NOT NULL` of any column;
- `=` and `<>` of a `text` or `varchar` column with a string, under a
  deterministic collation, one under which two strings are equal only
  when their bytes are; `= ''` and `<> ''` take the length alone;
- `LIKE` and `NOT LIKE` of such a column, under such a collation, with
  a constant pattern of fixed parts and at most 31 `%` signs, without
  `_` or an escape character.

The other side may be a constant, a parameter of a prepared statement,
a subquery's value or an outer row's value: an expression that holds no
column of the table and that Tessera lets a source use, which leaves
out a volatile function such as `random()`, a correlated subquery, and
a clause that must wait for a policy's or a security barrier's checks;
those go to `TessFilter`. A stored value that PostgreSQL's type cannot hold,
or bytes that are not UTF-8, compare as any other value and raise no
error unless the row passes and a consumer asks for the value.

Every other clause goes to `TessFilter` above the node, which checks it
in batches or row by row: for example `ILIKE`, a regular expression, a
`LIKE` with `_`, a comparison of two columns, a function of a column, a
comparison of floating-point numbers, of booleans or of the order of
strings, a `timestamp` compared with a `timestamptz`, and any clause on
strings under a nondeterministic collation. `EXPLAIN` shows the
clauses the source checks as `Source Exact` and those left as the
filter's `Filter`.

## Groups of rows skipped

Before the node reads a group, it asks the footer whether a clause the
source checks can pass any row of it. It skips the group, reading none
of its data pages, when the footer shows that none can:

- the count of `NULL` values decides `IS NULL` and `IS NOT NULL`;
- the least and greatest values of integers, dates and times, where the
  format lets a reader trust them, decide comparisons and `IN`;
- of strings, only whether the least or the greatest value is empty,
  for `= ''` and `<> ''`;
- where these do not decide, the chunk's dictionary, where the footer
  says that every data page of the chunk is encoded through it: the
  dictionary page is read alone, and the group is skipped when no value
  of it passes.

Within a group that is not skipped, every row is checked; the wrapper
uses no page index and no Bloom filter. How many groups a clause skips
depends on how the writer laid the rows out: a file written in the
order of a column gives each group a narrow range of that column.
`EXPLAIN ANALYZE` counts the groups skipped.

## Parallel reading

Where a query may run in parallel, the wrapper offers a partial path,
with the workers PostgreSQL gives a scan of the files' size
(`min_parallel_table_scan_size`, `max_parallel_workers_per_gather`).
The source offers its own beside it. The processes of the plan take
the groups of rows in turn from a counter in shared memory, so one file
is divided among them as a list of files is, and each row is read once.
A file of one group of rows is read by one process.

## Files that change

A scan reads the files as they were when it began. It notes each file's
size, time of modification, device and inode then, and checks the file
against them when it opens it and after each read. A file that changed
stops the query with an error, rather than give rows of two versions.
To replace a file under a running query, write the new one under
another name and rename it over the old: a scan that opened the old
file goes on reading it, and a scan that opens the new one sees another
inode and stops. A change that keeps the size and the time is not seen;
[What stays with the core](limitations.md#parquet-foreign-tables) lists
such changes.

Each backend keeps the footers it read, by path and identity, for up to
a thousand files, so that a query does not read the footers of many
files again.

## Estimates and ANALYZE

The planner takes a table's rows from its files' footers, as they are
when the query is planned. `ANALYZE` samples rows at random across all
the groups of all the files, reads only the rows it samples, and keeps
their statistics as for a heap table. Its sample, 30 000 rows by
default, falls in most pages of a wide file, so that `ANALYZE` decompresses most of
such a file once. Run it again when the files change much: the counts
of distinct values it stores grow with the rows, but the rest of the
statistics do not follow the files.

## What EXPLAIN shows

Both scans show the files and groups planned, the paths with `VERBOSE`,
and with `ANALYZE` the groups read. `TessParquetScan` also shows the
clauses it checks, and with `ANALYZE` the groups it skipped and the
PostgreSQL values it made. With `ANALYZE` the counts are summed over
the processes of a parallel plan and over the rescans of the node.

```
Custom Scan (TessParquetScan) on t_files
  Source Exact: ((a IS NOT NULL) AND (a > 1))
  Parquet Files: 7
  Row Groups: 14
```

```
Custom Scan (TessParquetScan) on t_files (actual rows=21.00 loops=1)
  Parquet Files: 7
  Row Groups: 14
  Row Groups Read: 14
  Row Groups Skipped: 0
  Values Decoded: 21
```

- `Source Exact`: the clauses the source checks, so that no filter
  checks them again.
- `Parquet Files`, `Row Groups`: the files and the groups of rows the
  plan found when it was made.
- `Parquet File Paths`, with `VERBOSE`: those files.
- `Row Groups Read`, with `ANALYZE`: the groups the scan read.
- `Row Groups Skipped`, with `ANALYZE`: the groups the footer ruled out.
- `Values Decoded`, with `ANALYZE`: the PostgreSQL values made, of all
  the columns; a string given as its bytes is not counted.

## Limits

[What stays with the core](limitations.md#parquet-foreign-tables)
lists what the wrapper does not read, what the source leaves to
others, and the changes of files a scan cannot see.
