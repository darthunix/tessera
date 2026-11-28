# string-bytes Specification

## Purpose
The bytes of a string column in a batch. A **batch** is a group of up
to 64 rows that Tessera's nodes pass from one to the next by column;
its **producer** is the node or source that made it, and its
**consumer** the node that reads it. Every batch gives each of its
columns as PostgreSQL `Datum` values, the form every consumer can read:
for a string, a `Datum` points to a value of its type, a header and
then the bytes. A source whose strings are not stored as such values,
as a Parquet file's are not, must make one for each row a consumer
asks, which costs a copy of the bytes and their check. This capability
lets a batch offer a column of `text`, `varchar` or `bytea` in a
second form, the **bytes form**: for each row, where its bytes are and
how many there are, and, where the producer holds the column through a
**dictionary**, a list of distinct values that each row names by its
**index**, the row's index too. The requirements go from the form, its
dictionary and the check of its bytes, to the batches that pass it on
and the Parquet source that offers it, and then to its consumers:
TessAgg's keys, the string kernels and TessSort's keys.
[design.md](design.md) explains the whole and the reasons.

## Requirements

### Requirement: The bytes form of a string column
A batch's operations MAY end with an optional operation,
`get_bytes_column`, after `release`; the size of the operations SHALL
tell a consumer whether it is there. A call SHALL take a column and a
mask of the rows asked, and SHALL either return false, the column not
offered in this form, or fill a structure of the layout below and return
true. A consumer SHALL read every column a batch does not offer in this
form, and every batch without the operation, through `get_datum_column`.
For a row asked that is not `NULL`, `data` SHALL point to the value's
bytes and `lengths` give their count: the bytes of the `text`, `varchar`
or `bytea` value without its header, never compressed and never stored
apart; the pointer of a row that is not `NULL` SHALL never be `NULL`, an
empty value's too. The pointers SHALL stay valid, and the bytes
unchanged, until the batch is released, whatever the consumer asks of
the batch in between; a consumer SHALL keep none of them past the
release. The caller SHALL set `struct_size`; the producer SHALL keep it
and fill the rest. Rows not asked SHALL hold entries of no meaning that
a consumer may load.

```
TessBytesColumn, 56 bytes, 8-aligned
offset size field
     0    8 struct_size   size of the structure the caller knows
     8    4 nrows         rows of the arrays: the batch's rows
    12    4 nvalues       the dictionary's count of values, or 0
    16    8 isnull        one bool a row: the row is NULL
    24    8 data          one pointer a row: the value's first byte
    32    8 lengths       one uint32 a row: the value's bytes
    40    8 indexes       one int32 a row: the index, or NULL
    48    8 dictionary    the dictionary's identity, 0 without one
```

#### Scenario: A column offered as bytes
- **WHEN** a consumer asks a test source's batch for a `text` column as
  bytes, for the rows of a mask, among them `NULL` rows, empty strings
  and strings of 1 to 10 000 bytes, then asks for another column and
  for the first one again as `Datum` values
- **THEN** the call returns true; each row asked holds its value's
  bytes and length, or its `NULL` mark; the pointers of the first call
  still give the same bytes after the later asks
- **Verified by:**
  `test/sql/source_scan.sql::-- spec: string-bytes/A column offered as bytes`

#### Scenario: A column or a batch without the form
- **WHEN** a grouping reads a test source's batches whose operations
  end at `release`, and then batches that return false for one text
  column and give another in the bytes form
- **THEN** it reads those columns through `get_datum_column`, and its
  groups are those of the same query over a heap table
- **Verified by:**
  `test/sql/source_scan.sql::-- spec: string-bytes/A column or a batch
  without the form`

### Requirement: The dictionary of the bytes form
Where the producer holds a column through a dictionary, it MAY give
`indexes`, `nvalues` and `dictionary`: each row asked that is
not `NULL` the index of its value, 0 or more and below
`nvalues`, the count of the dictionary's values, and
`dictionary` an identity, a number other than 0 that the producer took
from the operation of Tessera's API that gives them
(`tessera/bridge.h`), one counter for the backend; a producer of a
backend without that operation SHALL give no dictionary. Rows of one
index SHALL hold the same bytes; rows of two indexes MAY hold the same
bytes too, so a consumer SHALL NOT take two indexes for two values.
Batches that follow one another over the same dictionary SHALL give it
the same identity; a producer MAY give a dictionary a new identity when
it gives it again after another. No two dictionaries that batches of the
backend give SHALL share an identity, so that a consumer may keep what
it learned of an index for as long as the identity stays. An identity
SHALL NOT pass from one process to another. Without a dictionary,
`indexes` SHALL be `NULL`, and `nvalues` and `dictionary` 0.

#### Scenario: Batches over one dictionary
- **WHEN** a test source gives a column through one dictionary for
  three batches, then through another for the next three, and again
  after a rescan, beside a column it gives without a dictionary
- **THEN** the batches of one dictionary give the same identity and
  each row the index of its value, those of the next another identity,
  and those after the rescan one apart from the last before it; the
  column without a dictionary gives `NULL` indexes and the identity 0
- **Verified by:**
  `test/sql/source_scan.sql::-- spec: string-bytes/Batches over one dictionary`

#### Scenario: A dictionary given again
- **WHEN** a grouping reads the Parquet column of two chunks, whose
  dictionaries give one index to two values, each through its
  dictionary forward, a window of it behind the forward one, and the
  forward windows again
- **THEN** its groups are those of the core
- **Verified by:**
  `test/sql/parquet_cancel.sql::-- spec: string-bytes/A dictionary given again`

#### Scenario: Identities of one backend
- **WHEN** two modules of a backend take identities from Tessera's API
  in turn, and one also asks through a root of the size before the
  operation and through one whose operation is `NULL`
- **THEN** each identity is above 0 and above the one taken before it,
  and both of those asks get 0, so that their producer gives no
  dictionary
- **Verified by:**
  `test/sql/bridge.sql::-- spec: string-bytes/Identities of one backend`

#### Scenario: Dictionaries of two producers
- **WHEN** one backend reads the test source and the Parquet table, each
  through dictionaries
- **THEN** no identity of one producer is an identity of the other
- **Verified by:** review only — a producer's identities are not
  visible from SQL: the review lists each producer's call of
  `tess_dictionary_identity`, the bridge's one counter, which the
  scenario "Identities of one backend" shows to grow across modules

#### Scenario: Two indexes of one value
- **WHEN** a dictionary holds the same bytes at two indexes and a
  grouping by the column reads rows of both
- **THEN** the rows of both indexes fall into one group
- **Verified by:**
  `test/sql/source_scan.sql::-- spec: string-bytes/Two indexes of one value`

### Requirement: Bytes checked by their column's rule
A producer SHALL give in the bytes form only bytes that pass the check
a `Datum` of the column would pass when made for the same row: valid
UTF-8 without a zero byte for a `text` or `varchar` column whose rule
asks for the check, at most `n` characters for a `varchar(n)`, and no
longer than a value of PostgreSQL holds for every column. A row
asked whose bytes fail SHALL fail the call with the error and the
SQLSTATE that making its `Datum` would raise. A row not asked SHALL
not fail the call. A column whose rule trusts its bytes, as a Parquet
column of `utf8 'trust'`, SHALL give them unchecked, as its `Datum`
values are.

#### Scenario: Bytes that are not UTF-8
- **WHEN** a consumer asks a Parquet column marked for the check, which
  holds a value that is not UTF-8, in the bytes form for a mask of the
  rows before that value, then for a mask of its row, and then asks
  every row as `Datum` values after an ask of the bytes; the same of a
  column that holds a zero byte; and a grouping by each column reads it
  with Tessera on and off
- **THEN** the first ask of each returns its rows, and the others fail
  with SQLSTATE `22021`, the error of that value's `Datum`; both
  groupings fail with `22021`
- **Verified by:**
  `test/sql/parquet.sql::-- spec: string-bytes/Bytes that are not UTF-8`

#### Scenario: A value longer than its varchar
- **WHEN** a consumer asks a Parquet column of `varchar(3)`, which holds
  a value of four characters, in the bytes form for a mask of the rows
  before that value, then for a mask of its row
- **THEN** the first returns its rows, and the second fails with
  SQLSTATE `22001`, as the value fails when it is made as a `Datum`,
  and a grouping by the column with Tessera on and off fails with
  `22001` too
- **Verified by:**
  `test/sql/parquet.sql::-- spec: string-bytes/A value longer than its varchar`

### Requirement: The form passed on
A batch that a projection makes SHALL offer in the bytes form a column
it passes on unchanged from its child, where the child offers it, and
SHALL return false for a column it computes. A batch that a consumer
reads after a filter removed rows of its mask SHALL offer the form as
its producer does. The other batches Tessera makes SHALL NOT offer the
form: among them TessAppend's, over the partitions of a table or the
branches of a `UNION ALL`, and those that gather rows from other
batches, as a parallel plan's leader does from its workers. Whichever
form a consumer reads, a query's results SHALL be the same.

#### Scenario: Through a projection
- **WHEN** a grouping reads a test source through the source's
  projection, which passes one string column on and computes another,
  `lower(n)`, and groups by both
- **THEN** it reads the first as bytes and the second as `Datum`
  values, and its groups are those of a heap table
- **Verified by:**
  `test/sql/source_scan.sql::-- spec: string-bytes/Through a projection`

### Requirement: The Parquet source's bytes
The batches of the Parquet source (capability `parquet-fdw`) SHALL
offer every column of `text`, `varchar` and `bytea` in the bytes form,
pointing into the pages the source decoded, with no copy of the
bytes. A column of a group whose chunk the source reads through its
dictionary SHALL give the dictionary's indexes and identity. The count
of values the source made, in `EXPLAIN (ANALYZE)`, SHALL count the
`Datum` values alone.

#### Scenario: Indexes of a dictionary chunk
- **WHEN** a consumer asks a batch of a file read through its
  dictionary, and one of a file of plain pages, for their `text`,
  `bytea` and `varchar` columns in the bytes form, every other row and
  then every row, in batches of 64 rows and of 10
- **THEN** each row asked holds its value's bytes, the first ask's
  still after the others, and the rows through the dictionary carry
  indexes among the dictionary's values, one index a value
- **Verified by:**
  `test/sql/parquet.sql::-- spec: string-bytes/Indexes of a dictionary chunk`

#### Scenario: Plain and dictionary chunks
- **WHEN** a grouping by a `text` column, and a count of its distinct
  values, read a table of two files, one whose chunks are encoded
  through a dictionary and one of plain pages, with `NULL` rows and
  empty strings
- **THEN** its groups and counts are those of the core, and
  `EXPLAIN (ANALYZE)` of the scan counts no value made for the column
- **Verified by:**
  `test/sql/parquet.sql::-- spec: string-bytes/Plain and dictionary chunks`

### Requirement: Keys of a grouping by their bytes
TessAgg SHALL read a grouping key of `text`, `varchar` or `bytea` in the
bytes form where its input offers it and the key's collation is
deterministic, and SHALL then make the key's `Datum` values only for
the rows it writes to a spill file, from their bytes. Its groups, their
order where a sort above asks for one, and the rows its spill reads back
SHALL be those of the same grouping over `Datum` values. The ungrouped
`COUNT(DISTINCT)` of such a column SHALL read it the same way. A key of
a nondeterministic collation, and a column that is not offered, SHALL
be read as `Datum` values.

#### Scenario: Groups by bytes
- **WHEN** queries group the Parquet table of plain and dictionary
  chunks by a `text` column, by a `varchar` column and a number of a
  column before it, and by a `bytea` column, count each group, and
  count the distinct values of the `text` column with no grouping
- **THEN** their results are those of the core, and the scan counts no
  value made for the columns grouped by
- **Verified by:**
  `test/sql/parquet.sql::-- spec: string-bytes/Groups by bytes`

#### Scenario: Groups that spill
- **WHEN** a serial grouping by a `text` column of the Parquet table,
  whose chunks are encoded through a dictionary, has more groups than
  `work_mem` holds
- **THEN** its groups are those of the core, and its spill files are
  written and read back
- **Verified by:**
  `test/sql/parquet.sql::-- spec: string-bytes/Groups that spill`

#### Scenario: A partial stage's tables sent up
- **WHEN** the partial stage of a parallel grouping by that column has
  more groups than `work_mem` holds, so that it sends a table up and
  starts the next over the same chunk's dictionary
- **THEN** its groups are those of the core
- **Verified by:**
  `test/sql/parquet.sql::-- spec: string-bytes/A partial stage's tables sent up`

#### Scenario: A nondeterministic collation
- **WHEN** a grouping by a `text` column of a case-insensitive
  collation reads the Parquet table
- **THEN** its groups are those of the core, made from `Datum` values
- **Verified by:**
  `test/sql/parquet.sql::-- spec: string-bytes/A nondeterministic collation`

### Requirement: The bytes form in batch functions
A batch function (`tessera/function.h`) SHALL say that it reads its
string arguments in the bytes form by the flag `TESS_FUNCTION_BYTES`,
`0x20`, of its registration. A call, `TessFunctionCall`, SHALL hold a
field `bytes` after `result_scale`, which a function finds by the
call's `struct_size`: `NULL`, or an array of one pointer an argument.
To a function with the flag, a consumer SHALL give a string column its
input offers in the form with the argument's `column` `NULL` and its
pointer in `bytes` set; to a function without the flag, and for every
other argument, `column` or `scalar` as today, its pointer in `bytes`
`NULL`. `TessFunctionArg` SHALL stay as it is, since a call's arguments
are an array of it. The string kernels' entry points of `tessera/text.h`
SHALL take a column of strings as a `TessTextArg` with `column` or
`bytes` set, and a scalar with neither.

```
TessFunctionCall, 104 bytes, 8-aligned
offset size field
     0    8 struct_size   size of the structure the caller knows
     8    8 function      the function called
    16    4 nargs         the count of arguments
    20    4 (padding)
    24    8 args          nargs TessFunctionArg
    32    4 inputcollid   the input collation
    36    4 (padding)
    40    8 rows          the rows selected
    48    8 values        the results, or the aggregate's partial
    56    8 non_nulls     the rows of results not NULL
    64    8 context       where results by reference are made
    72    8 status        filled on failure
    80    8 decimal_rows  the rows of results that are decimals
    88    4 result_scale  the decimals' scale
    92    4 (padding)
    96    8 bytes         NULL, or nargs pointers to TessBytesColumn

TessTextArg, 24 bytes, 8-aligned
offset size field
     0    8 column        a column of Datum values, or NULL
     8    8 scalar        the scalar where neither column is set
    16    8 bytes         a column in the bytes form, or NULL
```

#### Scenario: With and without the flag
- **WHEN** a test module registers a function of a `text` argument
  with the flag and one without it, and a query calls both over the
  test source
- **THEN** the first gets the column in the bytes form and the second
  as `Datum` values, and both give the core's results
- **Verified by:**
  `test/sql/source_scan.sql::-- spec: string-bytes/With and without the flag`

### Requirement: String functions over bytes
The kernels of `length`, `char_length`, `octet_length`, `LIKE`,
`NOT LIKE`, `starts_with`, `=`, `<>`, `substr`, `left`, `right`,
`ltrim`, `rtrim` and `btrim` over `text` SHALL take a string argument
in the bytes form as well as a column of `Datum` values, and SHALL give
for it the results they give for the same values as `Datum`s. An
expression of Tessera SHALL pass a column it reads unchanged from its
input to such a kernel in the bytes form where the input offers it.

#### Scenario: Functions of a Parquet column
- **WHEN** queries over the Parquet table compute each of these
  functions of a `text` column, holding ASCII and multibyte UTF-8,
  `NULL` rows and empty strings, in a grouping's aggregate, in a
  filter and in an output column
- **THEN** their results are those of the core
- **Verified by:**
  `test/sql/parquet.sql::-- spec: string-bytes/Functions of a Parquet column`

### Requirement: A sort key of a string by its bytes
A collation SHALL **order by bytes** where PostgreSQL compares its
strings by their bytes alone: `C`, `POSIX` and the collations of the
builtin provider. TessSort SHALL order a key of `bytea`, and a key of
`text` or `varchar` of such a collation, ordered by its type's default
operator class of B-tree, by its bytes, as unsigned bytes, a shorter
string before every longer one it begins, reading the bytes form where
its input offers it. Where its input offers the form, a top-N sort SHALL
ask for the key's `Datum` values only for the rows that order before
the worst row it keeps when their batch is read. A key of another
collation or another operator class SHALL be sorted as today.

#### Scenario: Ordered by bytes
- **WHEN** queries over the Parquet table order by a `text` column
  under `C`, and under the builtin `C.UTF-8`, with multibyte UTF-8,
  strings that begin others or share their first eight bytes, `NULL`
  rows first and last, both directions, with and without `LIMIT 10`,
  serial and through a parallel plan's ordered gather; and by a `bytea`
  column holding the bytes `00`, `7f`, `80` and `ff`
- **THEN** their rows come in the core's order
- **Verified by:**
  `test/sql/parquet.sql::-- spec: string-bytes/Ordered by bytes`

#### Scenario: Top-N makes the kept values
- **WHEN** `ORDER BY` a `text` column under `C` with `LIMIT 10` reads
  the Parquet table of 100 000 rows whose order is not the key's
- **THEN** the scan's count of values made for the column is below
  what it reads, and the rows are the core's
- **Verified by:**
  `test/sql/parquet.sql::-- spec: string-bytes/Top-N makes the kept values`

#### Scenario: Another collation
- **WHEN** the same queries order under a libc collation other than
  `C`, and under `C` by an operator class of a test module that orders
  otherwise
- **THEN** their rows come in the order of that collation or class,
  sorted as today
- **Verified by:**
  `test/sql/parquet.sql::-- spec: string-bytes/Another collation`
