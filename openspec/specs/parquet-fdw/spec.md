# parquet-fdw Specification

## Purpose
A foreign data wrapper over Parquet files and its batch source. A
Parquet file keeps a table by columns: its rows come in **groups of
rows**, and in a group each column is a **column chunk**, compressed and
encoded apart, its pages often encoded through a **dictionary**, the
list of the chunk's distinct values kept once. The file's **footer**,
its metadata, gives every group's rows and every chunk's place, size and
often its least and greatest values, its count of `NULL` values and
which encodings its pages use. A value's **physical type** is how the
file stores it (`INT32`, `INT64`, `BYTE_ARRAY` and so on) and its
**mark**, the logical or converted type of the format, what it means (a
16-bit integer, a date, a string). A foreign table is a table whose rows
a foreign data wrapper reads from elsewhere; this one reads the files in
place. With Tessera's setting `tessera.enable`, the switch of all of
Tessera's planning, off, the wrapper returns rows, as any wrapper does.
With it on, the wrapper's source, a module registered in Tessera's
source registry (capability `source-scan`), reads the table in batches,
groups of rows kept by column, through a batch node of its own,
`TessParquetScan`, and checks some of the query's clauses itself, so
that Tessera's filter, `TessFilter`, checks only the rest. A batch
parent is a node of Tessera that reads batches from its child, such as
`TessAgg` or `TessSort`; over a child that returns rows it reads through
`TessPack`, which packs rows into batches. A **consumer** is the node of
a plan that reads the source's batches; a **window** is a run of rows of
one group of rows that the source reads together, many batches long; an
**injection point** is a named place in the code where a test module of
the suites can act, in a build with them. ClickBench is a benchmark of
analytic queries over one table of web traffic, 105 columns, whose
Parquet files keep times as bare integers and strings as unmarked bytes;
its types are the first the wrapper must read. The values come through
the read plans of the crate `datum-arrow` (capability `datum-arrow`),
from the Arrow types that Apache Arrow's Rust reader gives the files'
columns. The requirements go from the table and its columns to the rows,
the batches, the clauses, the groups of rows skipped, the parallel
reading, the estimates and `EXPLAIN`; [design.md](design.md) explains
the whole and the reasons.

## Requirements

### Requirement: A Parquet foreign table
`CREATE EXTENSION tessera_parquet`, which needs the extension `tessera`,
SHALL create the foreign data wrapper `tessera_parquet`. A server of the
wrapper SHALL take no options. A foreign table SHALL take one option,
`files`, required: a comma-separated list of absolute paths, each a file
or a pattern of the shell (`*`, `?`, `[…]`) whose matching files are
taken in the order of the bytes of their names. The patterns SHALL be
expanded when a query is planned and again when its scan begins, and the
scan SHALL read the files they match then. A column SHALL take the
options `column`, the name of the file's column it reads (its own name
by default), `unit` and `utf8` (the requirements "The columns and their
types" and "Text from bytes"). Setting or changing `files`, and any
change of a column's options whose list then holds `utf8 'trust'`, SHALL
take a superuser or a role with the privileges of the role
`pg_read_server_files`: the server reads the files with its own rights,
and a trusted column lets their bytes into `text` values unchecked. A
column with `utf8 'trust'` SHALL name its file column by the option
`column`, so that renaming the column cannot move the trust to another
file column. The wrapper SHALL read files whose chunks are uncompressed
or compressed with Snappy.

The wrapper SHALL refuse with an error: an option it does not know,
SQLSTATE `HV00D`; a table without `files`, `HV002`; a path that is not
absolute, an empty list, a value of `unit` or `utf8` it does not know,
or `utf8 'trust'` without `column`, `22023`; `files` set or changed, or
the options of a trusted column changed, by another role, `42501`; and,
when a query is planned or its scan begins, a path or pattern that
matches no file, `58P01`, and a chunk of a column the query reads
compressed another way, `0A000`. PostgreSQL itself refuses an option
given twice, `42710`.

#### Scenario: A table over files and patterns
- **WHEN** a role with the privileges of `pg_read_server_files`, not a
  superuser, makes a table that names a file and patterns of `*`, `?`
  and `[…]` that match two files each, of several groups of rows and of
  chunks uncompressed and compressed with Snappy, and a query reads it
  with Tessera off in a serial plan
- **THEN** it returns the rows of the seven files, the file first and
  then each pattern's two in the order of the bytes of their names, the
  groups of a file and the rows of a group in their order
- **Verified by:**
  `test/sql/parquet.sql::-- spec: parquet-fdw/A table over files and patterns`

#### Scenario: Options refused
- **WHEN** a server is created with an option; a table with an unknown
  option, without `files`, with `files` twice, with a relative path,
  with an empty list, and with a column of `unit 'weeks'`, of
  `utf8 'maybe'` and of `utf8 'trust'` without `column`; a role without
  the privileges of `pg_read_server_files` creates a table, changes the
  `files` of a table it owns, sets `utf8 'trust'` on one of its columns,
  and changes the option `column` of a trusted column of a table handed
  to it; and
  queries read a table whose pattern matches no file
  and one whose file is compressed with GZIP; and a query that reads
  only the columns of a file compressed with Snappy beside its one
  column compressed with GZIP
- **THEN** each raises its error, with the SQLSTATE listed above, and
  the query that reads only the columns compressed with Snappy returns
  its rows
- **Verified by:**
  `test/sql/parquet.sql::-- spec: parquet-fdw/Options refused`

### Requirement: The columns and their types
A column of a foreign table SHALL read the file's column of the name its
option `column` gives, or of its own name, compared exactly. Every file
of a table SHALL hold each column a query reads, of one value a row, not
repeated and not nested; a column a query does not read is not checked
for it, whatever its type, in a footer the format allows; a footer it
does not allow raises `XX001` for every query. A file column marked
optional SHALL give `NULL` where a row has no value; a required one
never does. The wrapper SHALL read a file by its physical types and
marks alone, never by a schema of Apache Arrow that a writer kept in the
file's metadata, and SHALL map them to the declared types in this list,
and only so, a column's logical type, where the file gives one, deciding
over its older converted type:

- `BOOLEAN` to `boolean`;
- `INT32` marked as a signed 8- or 16-bit integer, or as an unsigned
  8-bit one, to `smallint`, `integer` or `bigint`; unmarked, marked as a
  signed 32-bit integer or as an unsigned 16-bit one to `integer` or
  `bigint`; marked as an unsigned 32-bit one to `bigint`;
- `INT32` marked `DATE`, or with the column option `unit 'days'`, days
  since the Unix epoch, the first of January 1970, to `date`, the option
  taken only on a column without a mark of a date or a time;
- `INT64`, unmarked or marked as a signed 64-bit integer, to `bigint`;
  with the column option `unit` `'seconds'`, `'milliseconds'` or
  `'microseconds'` since the epoch's midnight, to `timestamp` or
  `timestamptz`, the option taken only on a column without a mark of a
  time;
- `INT64` marked `TIMESTAMP` in milliseconds or microseconds, to
  `timestamp` where the mark says the time is not adjusted to UTC and
  to `timestamptz` where it says it is; marked only with the older
  `TIMESTAMP_MILLIS` or `TIMESTAMP_MICROS`, which the format reads as
  adjusted to UTC, to `timestamptz`;
- `FLOAT` to `real` or `double precision`, `DOUBLE` to `double
  precision`;
- `BYTE_ARRAY`, marked as a string or unmarked, read as bytes either
  way, to `text`, `varchar` with or without a length, or `bytea`;
  `FIXED_LEN_BYTE_ARRAY` unmarked to `bytea`.

When a query is planned, and again when its scan begins, a column the
query reads that the file lacks SHALL raise an error, SQLSTATE `42703`,
and a file column of a type outside the list (such as `INT96`, a
decimal, a time of day, a time in nanoseconds, a list, a map or a group,
or bytes of another mark, such as JSON or a UUID), `0A000`. It SHALL
then refuse a column in this order: a declared type the wrapper does not
take, such as `numeric`, `char(n)` or `timestamp(0)`, `0A000`; a `unit`
or `utf8` on a declared type it does not apply to, or a `unit` on a file
column it does not apply to (one marked as a date or a time, `'days'` on
any but an `INT32`, a unit of time on any but an `INT64`), `22023`; a
declared
type the list does not give for the file's type, `42804`, also for a
`text` column over a file column that is not `BYTE_ARRAY`, with or
without `utf8`, since `utf8` applies to the declared type and the
wrapper checks bytes alone. A value made into a PostgreSQL value outside
the range of its type, such as a time that `timestamp` cannot hold,
SHALL raise an error, SQLSTATE `22008`, and one larger than PostgreSQL
holds, `54000`.

#### Scenario: ClickBench's types
- **WHEN** a table reads a file of 16-bit signed integers in `INT32`,
  days in an unsigned 16-bit `INT32` with `unit 'days'`, seconds in an
  `INT64` with `unit 'seconds'`, 64-bit integers and unmarked strings,
  with `NULL` values, as `smallint`, `date`, `timestamp`, `bigint` and
  `text`, with Tessera off and on
- **THEN** every row equals the row of a heap table of the same values
- **Verified by:**
  `test/sql/parquet.sql::-- spec: parquet-fdw/ClickBench's types`

#### Scenario: Every mapping of the list
- **WHEN** a table reads, through the option `column` where its names
  differ, a file of every other physical type and mark of the list:
  booleans, floating-point numbers, unsigned 8-bit integers as
  `smallint`, unsigned 16- and 32-bit ones, dates, times in milliseconds
  and microseconds adjusted and not adjusted to UTC and with the older
  marks and with both a logical type not adjusted to UTC and the older
  mark, strings marked and unmarked into `varchar` with and without a
  length, and fixed-length bytes, in required and optional columns, the
  file's metadata keeping a schema of Apache Arrow that names other
  types, with Tessera off and on
- **THEN** every row equals the row of a heap table of the same values
- **Verified by:**
  `test/sql/parquet.sql::-- spec: parquet-fdw/Every mapping of the list`

#### Scenario: A value larger than PostgreSQL holds
- **WHEN** a string column holds a value of more than a gigabyte
- **THEN** a query that makes its value raises `54000`
- **Verified by:** review only — a file with such a value is too large
  for the suites; the crate `datum-arrow`'s tests of values read show
  the check the wrapper raises

#### Scenario: Columns refused
- **WHEN** a query reads a table with a column its file lacks, a column
  whose name differs from the file's only by case, a column over an
  `INT96`, a decimal, a list, a time of day, a time in nanoseconds and
  bytes marked JSON, a `numeric` and a `timestamp(0)` column, a `unit`
  on a `text` column, a `utf8` on an `integer` one, `unit 'days'` on an
  `INT32` marked `DATE`, `unit 'seconds'` on an `INT64` marked
  `TIMESTAMP`, `unit 'days'` on a `date` over an `INT64` and `unit
  'seconds'` on a `timestamp` over an `INT32`, a
  `smallint` over an `INT64`, and two columns of two
  faults each, a `numeric` with `unit 'days'` and a `utf8` on a
  `smallint` over an `INT64`, and a `text` column over an `INT32`, once
  with `utf8 'check'` and once without; others read only the
  other columns, and count the rows, of a table with a column its file
  lacks, and of a file that also holds bytes marked as a decimal of
  precision 300, an `INT64` time of day in microseconds and a list, with
  Tessera off and on; and another reads a time in seconds past the
  range of `timestamp`
- **THEN** each raises its error, with the SQLSTATE listed above, the
  two of two faults `0A000` and `22023` by the order above, and the
  queries that do not read the missing or refused columns return their
  rows
- **Verified by:**
  `test/sql/parquet.sql::-- spec: parquet-fdw/Columns refused`

### Requirement: Text from bytes
A `text` or `varchar` column SHALL need a database of the encoding
`UTF8` when a query reads it; a query that reads one in another database
SHALL raise an error when it is planned, SQLSTATE `0A000`. Its column
option `utf8` SHALL take `'check'`, the default, or `'trust'`, which
only `text` and `varchar` without a length take; a `'trust'` on a
`varchar` with a length SHALL raise an error when a query is planned,
`42804`. With `'check'`, a value SHALL be checked as UTF-8 without a
zero byte, and as no longer in characters than a `varchar`'s length,
when it is made into a PostgreSQL value, whether the file marks its
column as a string or not: the wrapper's rows make every value of every
column the query reads, the source's batches only the values a consumer
asks for. A value that fails SHALL raise an error, SQLSTATE `22021`, or
`22001` for its length. With `'trust'`, no value SHALL be checked for
UTF-8 or a zero byte: whoever made the table promises that its strings
are UTF-8 and hold no zero byte. A `bytea` column SHALL take the bytes
as they are.

#### Scenario: Bytes checked when they become values
- **WHEN** a file holds a string that is not UTF-8 and one with a zero
  byte, in a column marked as a string and in one unmarked, in plain
  pages and through a dictionary whose value that is not UTF-8 only rows
  a clause rejects use, and queries read their column with `'check'`,
  through the wrapper's rows and through the source's batches, the
  batches once returning the rows and once under a clause the source
  checks that rejects them; with `'trust'`; and as `bytea`
- **THEN** the rows and the batches that return the rows raise `22021`,
  the batches under the clause return the other rows without an error,
  the dictionary's value used only by rejected rows raises none,
  `'trust'` returns the bytes as text, and `bytea` returns them as bytes
- **Verified by:**
  `test/sql/parquet.sql::-- spec: parquet-fdw/Bytes checked when
  they become values`

#### Scenario: The length of a varchar
- **WHEN** a `varchar(3)` column reads strings of three characters of
  two bytes each and of four characters, once returned and once in rows
  a clause the source checks rejects; and a `varchar(3)` column is
  declared with `utf8 'trust'`
- **THEN** the strings of three characters are returned, the string of
  four raises `22001` where it is returned and nothing where the clause
  rejects it, and the trusted column raises `42804` when it is planned
- **Verified by:**
  `test/sql/parquet.sql::-- spec: parquet-fdw/The length of a varchar`

#### Scenario: A database of another encoding
- **WHEN** a database of the encoding `LATIN1` reads a table with a
  `text` column, and one with only `bytea` and integer columns
- **THEN** the first raises `0A000` when it is planned, and the second
  returns its rows
- **Verified by:**
  `test/sql/parquet.sql::-- spec: parquet-fdw/A database of another encoding`

### Requirement: Reading rows
When the planner takes the wrapper's own scan, the wrapper SHALL return
every row of the files once, with the values the requirements on the
columns give: in a serial plan, the files in the order the requirement
"A Parquet foreign table" gives, the groups of rows of a file in their
order and the rows of a group in theirs; in a parallel plan, in the
order the participants return them; the core checks the query's clauses
on them. A rescan, each time the executor starts the scan anew, SHALL
return the rows again. A scan SHALL read the files as they are when it
begins, also under a plan made before they changed, and SHALL check its
columns against them again. A scan, serial or parallel, SHALL raise
`55000` when it opens a file whose size, time of modification, device or
inode differs from what it found when it began. It SHALL check a file's
size and time of modification after each read of it, before it uses what
it read, so that a file whose size or time of modification changes while
a scan reads it raises an error, SQLSTATE `55000`, before any value read
after the change is used; a file whose bytes the format cannot decode
SHALL raise `XX001`. A cancel of the query SHALL stop the reading within
a group of rows.

#### Scenario: Rows with Tessera off
- **WHEN** a table of several files and groups of rows is read with
  Tessera off: in full, by clauses, under a grouping, and as the inner
  side of a nested loop that scans it once for each outer row
- **THEN** each result equals the result over a heap table of the same
  rows
- **Verified by:**
  `test/sql/parquet.sql::-- spec: parquet-fdw/Rows with Tessera off`

#### Scenario: Files replaced, added, removed and damaged
- **WHEN** a prepared statement reads a table whose files are one
  pattern, its file is replaced by one of other rows and the statement
  runs again; then a second file that the pattern matches is added; then
  both files are removed; then one is put back, of a file that lacks a
  column the table reads; a cursor reads the first rows of a table of
  one group of rows longer than a window, its file is rewritten in place
  with the same size and a time set a minute later, and the cursor reads
  the rows of the pages it had not read; a serial scan of two files
  reads the first while the second is replaced by a rename with one of
  the same size and time; and a query reads a file whose footer is cut
  short and one whose page is damaged
- **AND** each of these runs once with Tessera off and once with it on,
  through the wrapper's rows and through the source's node
- **THEN** the second run returns the new rows, the third the rows of
  both files, the fourth raises `58P01`, the fifth `42703`, the cursor
  and the serial scan `55000`, and the damaged files `XX001`
- **Verified by:**
  `test/sql/parquet.sql::-- spec: parquet-fdw/Files replaced, added, removed
  and damaged`

#### Scenario: A cancel within a group of rows
- **WHEN** a query that reads one group of a million rows is cancelled
  when it has read the group's first window, through an injection point
  at each window read
- **AND** each of these runs once with Tessera off and once with it on,
  through the wrapper's rows and through the source's node
- **THEN** it stops with the cancel's error, `57014`, the injection
  point having counted fewer windows than the group holds
- **Verified by:**
  `test/sql/parquet_cancel.sql::-- spec: parquet-fdw/A cancel within a group of
  rows`

### Requirement: Batches through the source
With `tessera.enable` on, the module's source SHALL read every table of
the wrapper that the scan planner offers it (capability `source-scan`),
and return the path of its batch node, `TessParquetScan`, but for a
query whose targets or clauses read a whole row of the table, the table
as one value of its row type, or a system column such as `tableoid`:
the batch node computes columns alone, and the wrapper's scan reads such
a query. The node SHALL
make PostgreSQL values of a column only when a consumer of its batches
asks for that column, and only for the rows the consumer asks for. A
consumer SHALL get the right values whatever it asks, as many times as
it asks, for any rows of the batch, in any order, until it releases the
batch. A query that reads no column and has no clause, such as
`count(*)`, SHALL read no data page. The rows SHALL be those of the
wrapper's own scan; an error a value raises SHALL come only from the
values the node makes. The node SHALL keep the duties of the
requirement "Reading rows": the files as its scan begins, the check of
a file after each read, `55000`, the bytes it cannot decode, `XX001`,
and a cancel within a group of rows.

#### Scenario: A whole row and a system column
- **WHEN** queries read a table's whole row and its `tableoid`, with
  Tessera on
- **THEN** each plan reads the table through the wrapper's scan, and
  each result equals the result with Tessera off
- **Verified by:**
  `test/sql/parquet.sql::-- spec: parquet-fdw/A whole row and a system column`

#### Scenario: Batch parents over the source
- **WHEN** a grouping, a sort under a limit and a hash join read a table
  with Tessera on
- **THEN** each plan reads it through `TessParquetScan`, with no
  `TessPack`, and each result equals the result with Tessera off
- **Verified by:**
  `test/sql/parquet.sql::-- spec: parquet-fdw/Batch parents over the source`

#### Scenario: Columns made for the rows kept
- **WHEN** `SELECT *` over a table of ten columns and 100 000 rows is
  sorted under `LIMIT 10` by a column whose values ascend in the file,
  and another query counts the rows of a table whose data pages are
  damaged and whose footer is whole
- **THEN** the node makes a value of every row of the sort's column but,
  of each other column, only of the rows the sort took in, as `EXPLAIN
  ANALYZE` shows (the requirement "EXPLAIN"), and the rows equal those
  with Tessera off; and the count makes no value and returns the
  footer's count of rows without an error
- **Verified by:**
  `test/sql/parquet.sql::-- spec: parquet-fdw/Columns made for the rows kept`

#### Scenario: One column asked again
- **WHEN** a query computes `sum(a) FILTER (WHERE b = 1)` and
  `sum(a) FILTER (WHERE b = 2)`, and `CASE WHEN k % 64 = 63 THEN v ELSE
  v + 1 END`, so that a batch is asked for `a` and for `v` more than
  once, for rows that do not nest and in reverse order
- **THEN** each result equals the result over a heap table of the same
  rows
- **Verified by:**
  `test/sql/parquet.sql::-- spec: parquet-fdw/One column asked again`

#### Scenario: Values kept across the asks of a batch
- **WHEN** a consumer of the test module asks one batch of the node for
  an integer column, a time column and a `text` column, each with
  `NULL` rows, for row 63, then for rows 0 to 62, then for row 63
  again, keeping the arrays each ask gave
- **THEN** every ask gives the values and the `NULL` flags of the rows
  it asked, and the arrays of the first ask still hold row 63's value
  and flag after the second
- **Verified by:**
  `test/sql/parquet.sql::-- spec: parquet-fdw/Values kept across the asks of a
  batch`

### Requirement: The clauses the source checks
The source SHALL answer that it checks exactly (`TESS_CLAUSE_EXACT` of
`source-scan`) each clause in this list that Tessera marks usable, and
SHALL leave every other clause to `TessFilter`:

- a comparison, `=`, `<`, `<=`, `>`, `>=` of the types' B-tree family,
  the operators by which PostgreSQL's B-tree indexes order them, or
  `<>`, the negator of its `=`, the operator whose answer is the
  opposite, of a column of an integer type with an integer of any
  integer type, of a `date` column with a `date`, of a `timestamp`
  column with a `timestamp`, or of a `timestamptz` column with a
  `timestamptz`, where the other side holds no column of the relation;
- such a column `IN` a list, or `= ANY` an array, of such values;
- `IS NULL` and `IS NOT NULL` of any column;
- under a deterministic collation, one under which two strings are
  equal only when their bytes are, the equality or inequality of a
  `text` or `varchar` column with a string that holds no column of the
  relation, which `= ''` and `<> ''` answer by the length alone;
- under a deterministic collation, `LIKE` and `NOT LIKE` of such a
  column with a constant pattern of fixed parts and at most 31 `%`
  signs, without `_` or an escape character.

The source SHALL compute a side that is not a constant as `source-scan`
says, when its node is first asked for rows and after each rescan, and
SHALL check every row by the file's stored values, so that a row whose
value is out of range or not UTF-8 compares as any other and makes no
value unless it passes and a consumer asks for it. Each result SHALL
equal the result of the same query over a heap table of the same rows.

#### Scenario: Clauses checked by the source
- **WHEN** a table is filtered by each kind of clause in the list: on
  `smallint`, `integer`, `bigint`, `date`, `timestamp` and `timestamptz`
  columns, by a constant, a parameter of a prepared statement and a
  subquery's value, with `<>` among them and a `smallint` column against
  `bigint` constants outside its range; by `IN` and `= ANY`; by `IS
  NULL` and `IS NOT NULL`; by `<> ''`, `= 'x'`, `LIKE '%a%b%'`, `NOT
  LIKE 'a%'`, `LIKE 'ab%ba'` over rows `'aba'` and `'abba'`, and `LIKE
  'a%a'` over a row `'a'`
- **THEN** `EXPLAIN` shows each under the source's node and none in
  `TessFilter`, and each result equals the result over a heap table of
  the same rows
- **Verified by:**
  `test/sql/parquet.sql::-- spec: parquet-fdw/Clauses checked by the source`

#### Scenario: Clauses left to the filter
- **WHEN** a table is filtered by a regular expression, by a `LIKE` with
  `_` and one with 32 `%` signs, by a comparison of two of its columns,
  by a function of a column, by a `timestamp` column compared with a
  `timestamptz` under a time zone with daylight saving, and by a `LIKE`,
  an `=` and a `<>` under a case-insensitive nondeterministic collation,
  over strings that differ in their bytes and compare equal
- **THEN** `TessFilter` checks each, and each result equals the result
  over a heap table of the same rows
- **Verified by:**
  `test/sql/parquet.sql::-- spec: parquet-fdw/Clauses left to the filter`

#### Scenario: A clause on a value that changes by rescan
- **WHEN** the table is read with Tessera on in a correlated subquery,
  under a clause that compares a column with the outer row's value and
  under an `IN` of the outer row's values, so that the source's node is
  rescanned for each outer row
- **THEN** each result equals the result over a heap table of the same
  rows
- **Verified by:**
  `test/sql/parquet.sql::-- spec: parquet-fdw/A clause on a value
  that changes by rescan`

#### Scenario: Values out of range in a clause
- **WHEN** a file holds a time in seconds past the range of `timestamp`,
  and the source counts the rows that pass a clause on that column which
  rejects it, and then one which passes it
- **THEN** the first count is right and raises no error, and the second
  raises `22008` only where a consumer asks for the column's value
- **Verified by:**
  `test/sql/parquet.sql::-- spec: parquet-fdw/Values out of range in a clause`

### Requirement: Groups of rows skipped
For the clauses it checks exactly, the source SHALL skip a whole group
of rows, reading no data page of any of its columns, when what the
footer says of a column's chunk shows that no row of the group can
pass:

- its count of `NULL` values, where the footer gives it, which decides
  `IS NULL` and `IS NOT NULL`; a chunk without the count is taken to
  hold `NULL` values;
- for a chunk of integers, dates or times, its least and greatest
  values, where the footer gives them and the format lets a reader trust
  them: the fields `min_value` and `max_value` only where the file's
  column orders, the footer's word on the order each column's values
  are compared in, give the column the order of its type, and the older
  fields `min` and `max`, which a writer always sorted as signed, where
  the footer has no `min_value` and `max_value`, only for signed
  values;
- for a chunk of strings, and for `= ''` and `<> ''` alone, whether its
  least or its greatest value is empty, from the fields `min_value` and
  `max_value` only where the column orders give the column the order of
  its type: `= ''` passes no row of a chunk whose least value is not
  empty, and `<> ''` none of one whose greatest value is empty; `<> ''`
  is not tried against the dictionary of a chunk whose greatest value is
  not empty;
- for a clause other than `IS NULL` and `IS NOT NULL`, where these do
  not decide, its dictionary, where the footer's statistics of
  encodings say that every data page of the chunk is encoded through
  it: the dictionary is read alone, and the group is skipped when no
  value of it passes, since such a clause passes no `NULL` value.

Within a group not skipped the source SHALL skip no row by the footer.
The results SHALL be those without skipping.

#### Scenario: Groups skipped
- **WHEN** a table of eight groups of rows is filtered by a comparison
  that the least and greatest values of six groups reject; by a date
  that only one group's dictionary holds, in a file without least and
  greatest values for the column; by `= 2` on a group whose dictionary
  holds 1 and 3 and whose least and greatest values are 1 and 3; by `=`
  and by `LIKE` on a string that only one group's dictionary holds; by
  `IS NULL` over groups some of which hold `NULL` values and some none;
  by `IS NOT NULL` over groups some of which hold only `NULL` values; by
  a date that only one group's dictionary holds, in a file whose
  dictionary pages are headed `PLAIN_DICTIONARY`, as ClickBench's are;
  and, in a file whose strings are not encoded through a dictionary, by
  `<> ''` over groups one of which holds only empty strings, and by
  `= ''` over groups one of which holds an empty string
- **THEN** `EXPLAIN ANALYZE` shows six groups skipped, then seven, then
  the group skipped, then seven for each clause on the string, then the
  groups without `NULL` values, then the groups of only `NULL` values,
  then seven, then the group of empty strings, then the seven groups
  without one, and each result equals the result over a heap table of
  the same rows
- **Verified by:** `test/sql/parquet.sql::-- spec: parquet-fdw/Groups skipped`

#### Scenario: Groups the footer cannot rule out
- **WHEN** a table is filtered by a value found only in a group's data
  pages that fall back from the dictionary to plain pages; by a value
  outside the dictionary of a chunk whose footer has no statistics of
  encodings; by clauses on unsigned integers whose least and greatest
  values are in the older fields, with and without column orders, and
  on signed integers whose values are in the fields `min_value` and
  `max_value` without column orders; and by `IS NULL` on a chunk whose
  footer has no count of `NULL` values, whose every data page is
  encoded through its dictionary, and which holds `NULL` values
- **THEN** `EXPLAIN ANALYZE` shows no group skipped, and each result
  equals the result over a heap table of the same rows
- **Verified by:**
  `test/sql/parquet.sql::-- spec: parquet-fdw/Groups the footer cannot rule out`

### Requirement: Parallel reading
Where the relation may be read in parallel, the wrapper SHALL offer a
partial path, a plan's way to read the relation in parts, one part a
process, parallel-aware, with the workers PostgreSQL gives a scan of the
files' size; its participants, the processes of the parallel plan, SHALL
take the groups of rows of the files the leader's scan found in turn, so
that one file's groups are divided as several files' are, and each row
SHALL be read once over all the participants. Every participant SHALL
read the files the leader found, as the requirement "Reading rows"
checks them. A rescan of a parallel plan SHALL return the rows again,
of the same files. The source's partial path SHALL divide the groups so
too.

#### Scenario: One file in parallel
- **WHEN** a parallel plan reads a table of one file of eight groups of
  rows, with Tessera on and off, in full, under a count, under a
  grouping, and as the inner side of a nested loop that rescans the
  parallel plan for each outer row
- **THEN** each result equals the serial result, the plan's workers are
  those PostgreSQL gives a table of the files' size, and the groups read
  by the participants add up to eight for each scan of the plan
- **Verified by:**
  `test/sql/parquet.sql::-- spec: parquet-fdw/One file in parallel`

#### Scenario: A file replaced between participants
- **WHEN** a parallel plan's leader has begun its scan, and the file is
  replaced by a rename with one of the same size and time before a
  worker opens it
- **THEN** the worker raises `55000`
- **Verified by:**
  `test/sql/parquet_cancel.sql::-- spec: parquet-fdw/A file replaced between
  participants`

### Requirement: Estimates and ANALYZE
The wrapper SHALL estimate a table's rows as the sum of its files'
rows, and the rows of a scan by PostgreSQL's selectivity of its
clauses. `ANALYZE` of a table SHALL sample its rows across its groups
of rows and store their statistics, as for a heap table. The wrapper's
and the source's paths SHALL cost the bytes of the column chunks they
read and the rows of the files they read, so that the source's path over the
columns a query reads costs less than the wrapper's rows over the same
columns.

#### Scenario: Rows from the files
- **WHEN** a table of three files, each of several groups of rows that
  hold values of a column no other group holds, is planned before and
  after `ANALYZE`
- **THEN** the scan's rows equal the files' rows before, and after it
  `pg_stats` holds the table's columns, with values of every group
  among its common values or its histogram, and a clause's rows follow
  them
- **Verified by:**
  `test/sql/parquet.sql::-- spec: parquet-fdw/Rows from the files`

#### Scenario: The source chosen
- **WHEN** a query that the scan planner offers to sources reads a
  table with Tessera on, its clauses all checked by the source
- **THEN** the plan reads it through the source's node, not the
  wrapper's rows
- **Verified by:**
  `test/sql/parquet.sql::-- spec: parquet-fdw/The source chosen`

### Requirement: EXPLAIN
The wrapper's scan and the source's node SHALL show in `EXPLAIN` the
number of files (`Parquet Files`) and of groups of rows planned (`Row
Groups`), and, with `VERBOSE`, the files' paths. With `ANALYZE`, both
SHALL show the groups of rows read (`Row Groups Read`); the source's
node SHALL show the groups it skipped (`Row Groups Skipped`) and the
PostgreSQL values it made (`Values Decoded`), each summed over the
participants of a parallel plan. The source's node SHALL show the
clauses it checks (`Source Exact`).

#### Scenario: What EXPLAIN shows
- **WHEN** a table is explained plain, with `VERBOSE`, and with
  `ANALYZE` in a serial and in a parallel plan, through the wrapper's
  rows and through the source
- **THEN** each shows its lines as listed above, and the counts with
  `ANALYZE` equal the serial ones in the parallel plan
- **Verified by:**
  `test/sql/parquet.sql::-- spec: parquet-fdw/What EXPLAIN shows`;
  the parallel plan's counts in
  `test/sql/parquet.sql::-- spec: parquet-fdw/One file in parallel`
