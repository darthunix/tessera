## MODIFIED Requirements

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

#### Scenario: A chunk that leaves its dictionary
- **WHEN** a file's string columns, `text`, `varchar(3)` and `bytea`,
  have chunks that begin through a dictionary and go on in plain
  pages, with `NULL`s and values of 0, 12, 13 and a thousand bytes in
  each part, and a value that is not UTF-8 in the dictionary part of
  one chunk and in the plain part of another; and queries read them
  through the wrapper's rows and through the source's batches: whole,
  under a clause the source checks, grouped by the string, by
  `length`, and ordered under `C` with a `LIMIT`
- **THEN** each result equals the result over a heap table of the same
  rows, and the value that is not UTF-8 raises `22021` where a query
  returns its row and nothing where the clause rejects it
- **Verified by:** pending
