# parquet-fdw: how it is built

The extension `tessera_parquet` reads Parquet files in place, as
foreign tables. It is three things in one module: a foreign data
wrapper, which any PostgreSQL client can query and which returns rows;
a batch source of Tessera's source registry, which offers Tessera's
planner a batch scan of the wrapper's tables; and that scan's node,
`TessParquetScan`. Below them, a reader in Rust, the crate
`crates/tessera-parquet`, reads the files.

This page explains the idea and how it is built. What the part promises
is in its spec, [spec.md](spec.md), next to this page: the types and
the options, the clauses the source checks, the groups of rows it
skips, the errors and their SQLSTATEs, and what `EXPLAIN` shows. The
rules of its build that no test of its behavior shows are its
architecture contract, [contract.md](contract.md). The guide for its
users is [docs/parquet.md](../../../docs/parquet.md). Three neighbours
hold what the part leans on: `source-scan`, the contract between
Tessera's planner and a source; `datum-arrow`, the boundary of types
between PostgreSQL and Apache Arrow; and `string-bytes`, the form in
which a batch gives its strings as bytes.

## Background

**Parquet.** A Parquet file keeps a table by columns. Its rows come in
**groups of rows**, often hundreds of thousands each. In a group, each
column is a **column chunk**, stored as **pages**, each compressed
apart. A chunk may be **encoded through a dictionary**: its distinct
values are stored once, in a **dictionary page** at the chunk's start,
and each row as an index into them. A writer whose dictionary grows
past its limit **falls back** to plain pages for the rest of the chunk.
The **footer**, the file's metadata at its end, gives every group's
rows and every chunk's place and size. It often gives a chunk's
**statistics** too: its least and greatest values, its count of `NULL`
values, and which encodings its pages use. A value's **physical type**
is how the file stores it (`INT32`, `INT64`, `BYTE_ARRAY` and so on);
its **mark**, the format's logical or converted type, says what the
value means: a 16-bit integer, a date, a string.

**PostgreSQL.** A **foreign data wrapper** reads a foreign table's rows
from elsewhere through callbacks the core calls while it plans and runs
a query. A **path** is one way to read a relation, which the planner
weighs against the others by its cost; a **partial path** belongs to a
parallel plan, whose processes, its **participants**, divide the rows.
A `Datum` is the word in which PostgreSQL passes a value. To **make a
value** is to turn a file's stored value into the PostgreSQL value a
consumer gets.

**Tessera.** A **batch** is a group of up to 64 rows of a node, kept by
column, with a mask of the rows still taken. A consumer, the node that
reads a batch, asks it for a column as arrays of `Datum` values, for the
rows of a mask, as often as it needs, until it releases the batch. A
**batch parent** is a node of Tessera that reads batches, such as
`TessAgg` or `TessSort`; `TessFilter` checks the clauses a scan leaves.
A **source** is a module of the source registry that Tessera's scan
planner asks, for a foreign table, for a path of a batch node; for each
clause the source says whether it checks it exactly, so that
`TessFilter` checks only the rest. A **read plan** of `datum-arrow` is
a column's conversion from an Arrow type to a PostgreSQL type, checked
once before any value.

Two words are the part's own. A **window** is a run of 1024 rows of one
group that the node reads together, sixteen batches long. A group's
**selection** is a bit for each of its rows, set where every clause the
source checks passes.

**ClickBench** is a benchmark of 43 analytic queries over one table of
web traffic, 105 columns. Its Parquet files, written by parquet-cpp,
keep about 500 000 rows a group, Snappy pages, dictionary and plain
encodings, times as bare integers and strings as unmarked bytes. It is
the case the part was built for.

## The problem

1. **Wide files, narrow queries.** A query reads a few of 105 columns,
   and a clause or a top-N sort keeps a few of its rows. A scan that
   makes a PostgreSQL value of every row of every column it reads
   spends there what the columnar file saved: before the clauses were
   checked on stored values, ClickBench's count of the rows where
   `AdvEngineID <> 0` spent about 290 instructions a row, and DuckDB 24.
2. **Clauses without values.** The common clauses, such as `CounterID
   = 62` or `SearchPhrase <> ''`, can be answered from the stored
   integers and bytes, and often from the footer for a whole group.
3. **One large file.** ClickBench's main file is 14 GB in 226 groups. A
   reader that gives each process a file of its own leaves it to one.
4. **Values the file does not mark.** Times are bare integers; strings
   are bytes that no mark says are UTF-8.
5. **Files that change.** A file may be replaced or rewritten between
   the planning and the scan, or under the scan.
6. **Two languages.** PostgreSQL's interfaces and errors are C, and an
   error unwinds the stack with `longjmp`; the best reader of the
   format is a Rust crate, through whose frames no `longjmp` may pass.

## Goals and what they cost

- **One reader, two paths.** The wrapper's rows and the source's batches
  read through one reader and make values by one conversion, so that
  the plan the planner chooses never changes a result. The cost: the
  wrapper's rows make every value of every column a query reads, and
  may fail on a value the batches would never make.
- **A value only when asked.** The node makes a column's values only
  when a consumer asks for it, only for the rows asked, and keeps them
  until the batch is released. The cost: the node keeps, for each
  column, the window its reader holds and a bit for each row made; and
  a read of its batch can fail on a value that is not damaged, so its
  batches do not promise their values stored.
- **Clauses on stored values, exactly.** The source checks a closed
  list of clauses on the file's stored values, before any value is
  made, and answers for them exactly. The cost: the list holds only the
  pairs of types whose values share one unit, and the clauses on strings
  only under a deterministic collation.
- **Groups skipped by the footer.** A group whose footer shows that no
  row can pass is not read. The cost: a footer consistent with itself
  is trusted, as the format's statistics are.
- **One file divided.** The participants of a parallel plan take the
  groups of rows in turn, of one file as of many. The cost: a file of
  one group is read by one process.
- **The mapping says what a value means.** A column option gives the
  unit of a bare integer, and another decides whether bytes are checked
  as UTF-8, checked only on the values made. The cost: the options are
  set by hand, column by column, and `'trust'` is a promise.
- **No Rust frame under an error.** Every call into the reader returns
  a status, and the C side raises the error after the call. The cost:
  every output is memory the C side gives, and the reader calls no
  function of PostgreSQL.

## The whole in one picture

```
 a query over a Parquet foreign table t

 planning   GetForeignRelSize: the patterns expanded, each file's footer
            (the backend's cache), each column read mapped (datum-arrow)
            GetForeignPaths ──► ForeignPath serial, ForeignPath partial
                                     │
            scan planner (source-scan): offers each to the sources
                                     │
            the source: an answer for each clause, the path of
            TessParquetScan ──► TessFilter above, for the clauses left
                                     │
 execution  TessParquetScan          ▼
            ┌────────────────────────────────────────────────────────┐
            │ take a group (in parallel, from a shared counter)      │
            │ the footer rules it out? skip it                       │
            │   statistics first, then dictionaries                  │
            │ pass 1: the clauses' columns, every row, window by     │
            │   window, checked on stored values → the selection     │
            │ pass 2: batches of up to 64 selected rows of a window  │
            │   a column asked: its window read for the selection,   │
            │   values made for the rows asked, kept for the batch   │
            └────────────────────────────────────────────────────────┘
                                     │  C entry points, statuses
                                     ▼
            crates/tessera-parquet: files and footers, columns mapped,
            windows read through the parquet crate, tests of clauses,
            footers' statistics and dictionaries, values made through
            datum-arrow
```

With Tessera off, or where the planner prefers it, the wrapper's
`ForeignScan` reads each group window by window through the same
reader, makes every value of every column the query reads, and gives
the rows one by one; the core checks the clauses on them.

The module's C files follow this picture. `options.c` holds the
options and their privileges; `files.c` the patterns, the files and the
mapping of columns; `fdw.c` the wrapper's planning, costs and
callbacks; `scan.c` the scan of files and groups that both paths share,
its rows and its parallel list; `node.c` the source and its node;
`analyze.c` the sample of `ANALYZE`. The reader's entry points are
declared in `parquet/tessera_parquet_reader.h`.

## A module in C over a reader in Rust

The wrapper, the source and the node are C, since PostgreSQL's
interfaces for a wrapper, a custom scan and errors are C. The files are
read by the crate `tessera-parquet`, linked into the module as a static
library. The crate reads the format through the `parquet` crate of
Apache Arrow's Rust implementation, with its features `arrow` and
`snap` alone, and converts every value through `datum-arrow`. The module
uses Tessera's public C API alone, the headers under `include/tessera/`
and the runtime library, so that it builds against the installed
headers as a module of another extension would; the installed set of
the suites builds it so.

One crate of Tessera is linked by the reader: `tessera-kernels`, for
the pattern of `LIKE` alone. This is a decision with an end. It was:
the module promised to build against the installed headers and its own
reader alone. It is: the reader takes the kernels' pattern, `Like`, so
that the source and `TessFilter` match a string by one rule, and the
contract's rule names that crate as its one exception. It will be: the
exception ends when the pattern moves to a crate meant for use outside
Tessera. Why: a copy of the pattern in the reader would keep one rule
in two places, and the move is work of its own.

Refused:

- **The reader in C over Apache Arrow's C++ library.** A large C++
  dependency at build time, from packages Tessera does not ship:
  Tessera counts on no package of PostgreSQL's distributions. And its
  batches speak `Datum`, not Arrow's form in memory.
- **A reader of our own.** Parquet's encodings, levels and codecs are a
  format of their own, and the `parquet` crate is used by hundreds of
  crates. One exception: the plain values of a dictionary page, which
  the skipping of groups reads alone, are decoded by the reader's own
  code, since the crate's decoders of values sit behind its feature
  `experimental` (below).
- **The module as a part of `tessera_nodes`.** A source is a module of
  another extension, by `source-scan`'s contract; and the wrapper
  brings the reader's dependencies, which the nodes do not need.

## The reader

### Files and their footers

A [`ParquetFile`](../../../crates/tessera-parquet/src/file.rs) is a
file opened, its footer read, and its **identity**: the size, the time
of modification, the device and the inode that `fstat` of the open
file gave. Every reader of one file reads at an offset through the one
descriptor, so that no reader opens or duplicates one. The C side
counts the descriptor against the backend's open files, since the
reader opens it outside PostgreSQL's own.

A backend keeps the footers it read, by path and identity, for a
thousand files at most, the least recently used dropped first, so that
a query over a hundred files does not read a hundred footers again. A
file replaced by a rename has another inode, so its footer is read
anew even with the same size and time. The cache is the reader's one
object that outlives a query, and the contract names it so.

A footer whose groups' rows do not add up to the file's is refused when
it is read. Any other fault of a footer shows where a query reads what
it describes.

### Columns mapped through datum-arrow

[`map_column`](../../../crates/tessera-parquet/src/column.rs) maps a
column of a foreign table to a column of a file: it finds the file's
top-level column by its exact name, takes its physical type and mark,
the logical type deciding over the converted one, refuses a codec
other than Snappy or none in any of its chunks, and makes the
`datum-arrow` read plan from the Arrow type to the declared type, with
the options `unit` and `utf8`. The order of these checks is the order
of the spec's errors; the list of the pairs taken is the spec's.

The `parquet` crate reads a file through a footer and an Arrow schema
it is given. The reader gives it, for each mapped column, a footer of
that column alone: a schema of one leaf, each group with its one chunk,
and no Arrow schema that a writer kept in the file's metadata, so that
a column's Arrow type comes from its physical type and mark alone. A
column the query does not read is not in any footer the crate sees, so
a column of a mark the crate cannot describe fails only a query that
reads it. A column of bytes loses its mark in that footer: the crate
checks UTF-8 itself in every page it decodes of a column marked as a
string, which would fail on rows a clause rejects and on a column the
table trusts. The checks of UTF-8 are `datum-arrow`'s, on the values
made. A chunk of `BYTE_ARRAY` read plain is asked for as Arrow's
`BinaryView`. A **view** is a record of 16 bytes a row: a value of up to
12 bytes lies in it, and a longer one is its length and its place in
one of the array's buffers. The buffers are the decompressed data pages
and the dictionary page themselves, so that no value is copied, the
rows a chunk reads through its dictionary before it falls back to plain
pages among them: each of those is a copy of the dictionary's view, not
of its bytes. A chunk read through its dictionary is asked for as a
dictionary of `LargeBinary`, whose offsets are 64-bit: its values are
copied once a chunk, and its values of one window may pass the 2 GB
that 32-bit offsets hold even when each value fits PostgreSQL.

Refused:

- **The file's whole footer given to the crate.** A column of a type or
  mark the crate cannot read would fail every query of the table.
- **Marked strings read as Arrow's `Utf8`.** The crate would check every
  value it decodes, those no consumer asks for and those of a trusted
  column; and `datum-arrow` would see two types for one kind of column.
  `Utf8View` is refused for the same reason: the crate checks a whole
  page at once and fails with an error of its own, not `22021` at the
  plan's row.
- **Plain chunks as `LargeBinary`.** The crate copies each value's bytes
  out of its page into the array's buffer, the rows read through the
  dictionary too: a tenth of a query that reads a column of strings in
  full.
- **The reader's own decoding of the pages of `BYTE_ARRAY`.** It would
  give the same places in the pages as the views, but needs the crate's
  decoders behind its feature `experimental` and checks of damaged pages
  of its own.
- **Views for every chunk.** The crate gives no dictionary whose values
  are views, and a chunk read through its dictionary would lose the
  indexes by which TessAgg numbers its rows and the clauses answer once
  a value.

### Windows read for a selection

A [`ColumnReader`](../../../crates/tessera-parquet/src/reader.rs) reads
one column of one group, for a selection of its rows, window by window.
It builds the crate's record batch reader from the footer already read,
for its group, its column and a selection of rows, over pages the
reader itself gives (see "Pages and Snappy"), under the crate's policy
of selectors: a row not selected is skipped, a whole page without
decompressing it where the skip covers the page.

A column asked window after window, as a scan in full asks it, keeps one
**forward reader** over the group with the group's selection, so that
each page is decompressed and the dictionary decoded once a group. Its
arrays hold a window's count of selected rows, so it may read into the
next window; it keeps what it read past the window for the next ask, as
a slice of the array it was read in. A window that one array holds is a
slice of it, and one that two arrays hold joins their slices. Not a
slice of the joined window: a slice of an array of views keeps every
buffer of its array, and a join every buffer of its parts, so a slice of
the join would carry each page read before into every later window, and
copy a list of buffers that grew a window at a time. A window after a
**gap**, past windows no consumer asked the column for, builds the
forward reader anew from that window, which skips the pages of the gap;
the windows after it are forward again. An ask **behind** the forward
reader, a window already passed, as a top-N sort asks for the rows it
kept, builds a reader of its own for that window and the rows asked.
Such a reader decodes the dictionary again and walks the headers of the
pages before the window, but decompresses only the pages that hold the
rows, and leaves the forward reader as it was.

A chunk of strings with a dictionary page is read as Arrow's dictionary
array, the dictionary and each row's index, so that a clause is checked
once a dictionary value. A chunk whose footer says that some data page
is encoded otherwise, as a chunk that fell back to plain pages, is read
plain, as views: read as a dictionary, the crate would build a
dictionary of each array of its plain pages, hashing every value, and
merge the dictionaries of a window's arrays, hashing again. A chunk
whose footer does not say how its pages are encoded is read through its
dictionary, which is right for a chunk that does not fall back.

The window's size, 1024 rows, sets two costs against each other. A
larger window makes a consumer that asks for one batch of it read the
rest of the window too; a smaller one makes a top-N sort, which asks
for windows out of order, build more readers, each decoding the
dictionary and walking the page headers again.

Refused:

- **One record batch reader over all the columns asked.** It decodes
  every column of every row before the consumer knows which rows it
  keeps: a top-N sort over `SELECT *` would decode the 105 columns of
  every row.
- **A whole column chunk decoded at its first use.** A top-N sort that
  takes a row from many groups would decode whole chunks of every
  column for a few rows.
- **The crate's array readers, which read and skip forward.** They sit
  behind its feature `experimental`, which brings the crates of
  Parquet's `variant` type.
- **A reader for the rows asked after a gap.** Each batch of the windows
  after it would build one and decompress its page again, so that one
  early window where a clause above the scan passed no row would leave
  the rest of the group read 64 rows at a time. Nor is a gap read and
  dropped: that decodes what no one asked for, where a new forward
  reader skips it.
- **A reader for each column and window.** Each reader starts at the
  chunk's start: over a scan in full it would decode the dictionary and
  decompress the page under each window's start once a window.

### Pages and Snappy

The `parquet` crate decompresses a page by the codec of its chunk's
compression, and its builder of readers takes no other. The reader
therefore builds each read through the crate's lower entry,
`try_new_with_row_groups` of `ParquetRecordBatchReader`, over row
groups and page readers of its
[own](../../../crates/tessera-parquet/src/pages.rs): one group, and for
each chunk the crate's page reader, over the chunk's metadata marked
uncompressed when the chunk is compressed with Snappy, so that it hands
the pages over as the file stores them. The reader decompresses each
one, the levels of a page of the format's second version kept as they
are, and gives it on, to the size the page's header gives, as the
crate's codec does: a page of another size is an error. A copy of the
file of the page reader's own keeps the first 12 bytes of the last
header it read, and the reader takes the size from them, field 2 of the
header in Thrift's compact form: the crate reads an index page's header
and the next page's in one call, and the page is checked against its
own. A header whose first fields are
written otherwise, which no writer known does, leaves the size
unchecked. A chunk without compression keeps the crate's page reader.
The dictionary page read for a column's keys takes the same way.

The crate reads a page's header through the file's reader from an
offset, and the page's data apart, by one read of its length. Its
Thrift reader reads a header's field markers, varints and numbers a
byte at a time, so the reader from an offset is a buffer of 1 KiB over
the file, as the crate's own reader of a file is: a header of up to a
kilobyte costs one system call, where a call for each field cost a
quarter of a query that skips most of a column's pages by their headers.
The buffer gives a read no more bytes than it asks, so the crate's
count of a header's bytes, where the page's data starts, holds. A
larger buffer reads more of each page's data, which the data's own read
reads again, for each page skipped too; neither 256 bytes nor 8 KiB
took 2 per cent fewer cycles.

```text
  the crate's page reader
    | a header: from its offset        | the data: its offset, its length
    v                                  v
  a buffer of 1 KiB                  one read of the file
    | one read a kilobyte
    v
  the file, a copy of which keeps a header's first 12 bytes
```

The lower entry takes no policy of row selection, and the crate's
default reads a selection whose runs are short on average by a mask:
every row of a batch's span decoded and filtered, a long skip inside it
too, so that one batch might decompress and hold the pages of most of a
group. The reader appends to each selection a skip that the policy
counts when it chooses and the plan trims before it reads, which makes
it choose selectors. The choice leans on that order of the crate's
private steps; a test sees the pages of a long skip left undecompressed
after short runs, and fails if a release of the crate changes it.

```text
  ParquetRecordBatchReader              the crate's
    '- the reader's row groups          one group of rows
         '- the reader's page reader    a Snappy chunk: each page
              |                           decompressed by the reader
              '- the crate's page       over the chunk's metadata
                 reader                   marked uncompressed
```

The decoder, the module
[`snappy`](../../../crates/tessera-parquet/src/snappy.rs), ports the
decompression of a whole buffer of Google's snappy 1.2.1. A Snappy
stream is its length as a varint, then elements: a literal, whose
bytes follow its tag, or a copy of bytes written before, at an offset
back. The crate `snap`, which the crate's codec calls, branches on each
element's kind and mispredicts some forty branches a kilobyte on the
pages of strings; Google's loop does not, and takes half the cycles.

```text
  input   [len][tag|data][tag|data] ...                [tag|data]
           |   '--------- fast loop --------------''--careful--'
           v
  output  [.......................................][...........]

  fast loop, two elements a turn, while 130 bytes of input remain
  and 128 of output:
    entry = TABLE[tag]          the length less the offset's bits
    one comparison              literal or copy, no branch on the kind
    the element before          copied now, as 32 or 64 bytes at once
    this element                waits for the next one
  careful loop: one element, every length and offset checked
```

- **The fast loop** takes literals of up to 60 bytes and copies of 1 and
  2 bytes of offset. A table of 256 entries gives each tag's length less
  the bits of its offset that the tag holds, so that one subtraction
  both tells a literal from a copy whose offset is below its length and
  gives the offset; a literal gets a false offset of 256. The next tag
  and the source of a copy are chosen by conditional selects, and each
  element's bytes are moved as 32 or 64 at once into room that later
  elements overwrite. A copy of a pattern shorter than its length is
  written by a shuffle of bytes, NEON's on AArch64 and SSSE3's on a
  build that targets it, by plain copies otherwise. The position in the
  input is a pointer, not an index: an index puts an addition on the
  chain from one tag's load to the next, and cost a tenth of the
  loop's cycles.
- **The careful loop**, in safe Rust, decodes one element, every length
  and offset checked before it is used: a long literal, a copy of 4
  bytes of offset, a copy from before the output's start or of an
  offset of 0, and the rest of a stream near its ends. A stream it
  cannot decode is an error, and the read raises `XX001`.
- **The bounds** of the fast loop are its guard: a turn starts only with
  130 bytes of input past its tag and 128 of output past what is
  written and the copy that waits. Kani proves that a turn from any
  state the guard admits stays inside its buffers and leaves a state the
  next turn may assume, the source of the copy that waits among it, and
  that the copy after the loop stays inside them too; a debug assertion
  checks the guard's room before every turn of every test; a fuzz target
  compares the decoder with `snap`'s on any bytes under
  AddressSanitizer.

Refused:

- **A crate of the workspace in place of `snap`,** through Cargo's
  patch of a dependency. Such a crate cannot depend on the real `snap`
  of crates.io, two versions of one crate from one source, which Cargo
  resolves for the whole workspace but refuses for the reader alone;
  the real crate from another source, a copy or a dependency of git,
  would serve, at the cost of keeping it.
- **A fork of `snap`:** its 2 500 lines to format, lint, mutate and
  keep in step, to change one file.
- **Google's snappy in C++:** a compiler of C++ and its library in the
  extension, outside Miri, Kani and the mutants, and slower when built
  by GCC, Linux's compiler by default, by DuckDB's own note.
- **The crate's default policy of row selection:** a selection of short
  runs read by a mask, with fewer instructions and about as many cycles
  on ClickBench's queries, but a long skip after them decoded whole.
- **A page's size unchecked against its header:** a page whose stream
  is valid but whose header says another size would be read, where the
  crate refused it.
- **Headers parsed by the reader,** from bytes it reads with a page's
  data: a parser of Thrift's compact form for the whole header, reads
  of a fixed length kept within the file's end, and the crate's page
  reader, which without a page index has no other way to find its
  pages, no longer reading them.

### Values made into the C side's memory

The values are made in Rust, by the reader's entry points, through the
conversions of `datum-arrow`, into memory the C node allocates. For a
type passed by value the node gives an array of words, and the reader
writes each row's `Datum` and its bit of `NULL`. For a string the reader
first checks the rows asked and says how many bytes their values need;
the node allocates an arena of that size in the batch's memory; the
reader then writes each value with its header, the short one of one
byte for a value of up to 126 bytes and the aligned one of four bytes
otherwise, and its `Datum`, the value's address. So a batch's strings
cost one allocation an ask, not one a row, and no call into C allocates
under a Rust frame.

A consumer may ask a batch for one column several times, for rows that
do not nest, in any order. The node keeps the values it made and a bit
for each row made, and asks the reader only for the rows not made yet.
What an earlier ask made stays, by what `datum-arrow`'s read promises:
for a plain column of integers, floating-point numbers, days, times or
booleans it may write the place of a row not asked, but only with that
row's own value where the value converts; for strings, bytes and
dictionaries it writes the places of the rows asked alone.

The arrays a batch gives are the scan's, not the batch's. For each
column the plan reads, by the scan's number of the column, the node
keeps an array of 64 values, one of 64 flags and a view of the bytes
form, allocated zeroed in the query's memory at the column's first
ask, in the form asked. Each batch, and a rescan, clears each column's
rows made and each view's dictionary, and writes nothing else. The rows
a batch does not make thus keep an earlier batch's values, which the
batch's contract allows: every row below the count is written memory,
by the first zeroing, and holds a value of no meaning where it was not
asked; an asked `NULL` row of the bytes form gets a place, a length and
an index of 0. Arrays made and zeroed at each batch, an entry for each
attribute of the table, cost a query that reads no column half its
cycles: ClickBench's `COUNT(*)` over its 105 columns. Under assertions
the node wipes the values and the bytes' places at each batch, as
PostgreSQL wipes freed memory, so that a consumer that reads a column
kept past its batch reads the wipe where the next batch has not made
the row again.

### Errors and panics

Every entry point that can fail returns true, or false with a status:
a SQLSTATE, a message and the row of the value that failed. The C side
raises the error after the call has returned. Each of those entry
points runs under `catch_unwind`: a panic while the `parquet` crate
decodes a file is reported as bytes the format cannot decode, any other
as an internal error. The others, the frees of a handle, the answers
about a pattern or a comparison and a count of references the tests
read, take no status and decode no file. A read that fails on a file
that changed since it was opened is reported as the change, not as
damaged bytes.

## The node and its batches

### Planning through the source

The source knows the wrapper's paths by the wrapper's callback that
made them. For a path offered, serial or partial, it answers "exact"
for each clause of its list that Tessera marks usable, and carries
those clauses in its path's expressions, as `source-scan` asks. It
refuses a query whose targets read a whole row of the table or a system
column, such as `tableoid`: the runtime's projection computes columns
alone, as the heap scan leaves such a query to the core's scan. The
wrapper's scan then reads it.

The node finds a clause's shape again when it begins, from its plan's
expressions, and fails as a fault of the plan if it cannot. It orders
the clauses by their column, so that a window reads each column once.

### Two passes over a group

The node reads a group in two passes. The first reads the columns of
the clauses it checks, forward over every row of the group, window by
window. On each window it applies each clause's test to the stored
values, a bit a row, and drops the values; a window that no row passes
stops its clauses early. The group's selection is whole before its
first batch is published, and a group of no selected row is passed
over.

The second pass publishes batches of up to 64 selected rows, each from
one window. A batch keeps each row's place in its window, or only the
first where its rows follow each other. Every other column is read only
when a consumer first asks for it in a batch, for the window's selected
rows. A window that no consumer asks a column for is never read for
it. A clause's column that a consumer asks for is read again, for the
selected rows alone.

A forward read must hold every selected row of its window; one that
holds fewer is a damaged file. A query that reads no column and has no
clause, such as `count(*)`, publishes the rows the footers count and
reads no data page.

The node's batches leave the flag `values_stored` of `tessera/batch.h`
false: a read of them can fail on a value that is not damaged, such as
a string that is not UTF-8. A parent that gathers sparse batches must
then not ask every column of every row before it drops rows. The node's
batches are full already, since a window publishes its selected rows
64 at a time. Under a parent that reads rows, the node serves the rows
of its batches one by one.

### The bytes form

The node's batches also give their `text`, `varchar` and `bytea` columns
as the bytes the window's arrays hold, in the decompressed pages or in
the views, with a dictionary's indexes, for a consumer that reads
strings so, such as a grouping by a string. The reader writes each row's
place and length, and a reference that keeps the window's array for the
batch, which the batch's memory frees. That form, its promises and its
checks are the capability `string-bytes`; this part only offers it.

## The clauses checked on stored values

The spec lists the clauses the source checks. The reader checks each
on a window's Arrow arrays, before any value is made, through a test
made once a file from the clause's values and the file's column, since
the file's column decides the unit the values compare in.

- **Comparisons and `IN`.** A comparison of an integer, a date or a
  time goes through the test that `datum-arrow` makes from the constant
  in the file's unit. The test is exact over every stored value, so
  that a stored value out of PostgreSQL's range compares as any other
  and fails no row by itself. `IN` is a sorted list of such bounds,
  searched for each value.
- **Strings.** Equality with a constant compares the length first and
  the bytes after; `<> ''` takes the length alone. `LIKE` with fixed
  parts and `%` takes the kernels' `Like`: each part found in order, the
  first anchored at the start and the last at the end unless a `%`
  stands there, so that `'ab%ba'` rejects `'aba'`. On a dictionary
  array a clause is checked once a dictionary value, and the rows take
  their index's answer. The answers are kept for the chunk's
  dictionary, which every window of the chunk shares over the same
  buffers: the test knows the dictionary by the addresses of its
  offsets and bytes, and holds the array while it keeps the answers, so
  that no other dictionary can take those addresses.
- **Collations.** The node takes a clause on a string by the function
  of its operator, `texteq`, `textne`, `textlike` or `textnlike`, under
  a deterministic collation alone: under such a collation the core's
  equality and `LIKE` of a pattern of fixed parts compare bytes too. A
  `varchar` column comes through the relabeling to `text` its operators
  take. A pattern must be a constant; one the kernels' pattern refuses
  is left to `TessFilter`, as is `ILIKE`.
- **Values known at execution.** A side that is a parameter, a
  subquery's value or an outer row's is computed through PostgreSQL's
  evaluation of expressions when the node is first asked for rows and
  after each rescan, as `source-scan` asks; the tests are then made
  again.

Which comparison a clause asks is found in two steps. The node asks
PostgreSQL's catalog for the operator's strategy in the column type's
B-tree family, the number that says "less", "equal" and so on, and for
`<>` the strategy of its negator. The reader then turns the strategy,
the order of the operands and the pair of types into a comparison of
the column with the other side, or refuses the clause. The rule is
small and closed, so a Rust test runs it for every strategy, negator,
order and pair of a list of types, and checks each answer against the
operator's meaning on values; kept in C, it would be judged by the
suites alone, since only Rust tests judge a mutant.

A `timestamp` compared with a `timestamptz` is left to `TessFilter`:
PostgreSQL turns each row's time into the session's zone, which no
single bound can stand for across a change of daylight saving. The
checks raise no error and write no message, so that a row a policy
hides is never shown by the source that rejected it.

The rest is proved or compared. Kani, a checker that proves a property
of a Rust function for every input, proves in `datum-arrow` each
comparison's bound exact. In the reader it proves the bounds of `IN`
the union of its values' tests of equality, for lists of two values.
Property tests compare each test of strings, over plain and dictionary
arrays and their slices, with a match of the pattern one byte at a
time. The suites compare the rows of every kind of clause with the same
query over a heap table of the same rows.

Refused:

- **The answer "prune only" for these clauses.** `TessFilter` would
  check them again on values the source made only for it.
- **The rule of strategies in C.** It would be judged by the suites
  alone.
- **A pattern of `LIKE` of the reader's own.** One rule of matching
  would live in two places.

## Groups of rows skipped by the footer

When a group starts, the node asks the reader, for each clause, whether
the footer rules the group out: first by the statistics of every
clause's column, which cost no read, and then by the dictionaries, so
that a dictionary page is read only where no statistics decide. A group
ruled out is counted as skipped, not read. The reader decides; the node
only asks. The spec gives the rules; the ideas behind them are these.

- **Counts of `NULL` values** decide `IS NULL` and `IS NOT NULL`. Every
  other clause the source checks passes no `NULL`, so for them a chunk
  of `NULL` values alone is empty.
- **Least and greatest values** are trusted as the format lets a
  reader trust them. The newer fields hold the order of the column's
  type only where the file's column orders say so; without them their
  order is undefined. The older fields were always sorted as signed by
  writers, so they serve signed values alone. A bound outside the
  width of the column's mark, such as 300 in a column marked as an
  8-bit integer, gives no bounds: such a row is read narrowed to the
  mark's width, and the bound would not hold for it.
  The bounds go through the same tests of `datum-arrow` the rows do.
- **Strings** are tried by their least and greatest values for their
  emptiness alone, for `= ''` and `<> ''`, and only from the newer
  fields under the column orders: older writers wrote wrong least and
  greatest values of bytes. Every order of bytes puts the empty string
  first, so a least value that is not empty, even one a writer cut
  short, shows that no row is empty, and a greatest value that is empty
  shows that every row is. A greatest value that is not empty leaves
  `<> ''` untried against the dictionary: the format lets a writer give
  a greatest value past every row, so it does not prove that a row
  passes, but a group it does not skip is read and checked, and no row
  comes out wrong. ClickBench's `SearchPhrase <> ''` is decided so in
  every group, where the dictionary would be read and decompressed,
  some 470 000 bytes a group, to skip nothing.
- **Dictionaries** are tried only where the footer's statistics of
  encodings say that every data page of the chunk is encoded through
  the dictionary, since a writer may fall back to plain pages. The
  reader takes the crate's default form of those statistics, a mask of
  the data pages' encodings; a list of every page, which the crate
  builds only on request, would cost a vector for each chunk of every
  file. Where the footer does not say, the dictionary is not tried. The
  dictionary page is read alone, and its plain values are decoded by
  the reader's own code, whether the page's header says `PLAIN`, as the
  crate's writer writes it, or `PLAIN_DICTIONARY`, as writers of the
  format's first version do. A page of another encoding leaves the
  dictionary untried, without an error. In ClickBench's files, 198 of
  the 210 chunks of a file of two groups are encoded through a
  dictionary in every data page, and their dictionary pages are headed
  `PLAIN_DICTIONARY`.

A footer that contradicts itself rules nothing out: neither a range
turned the wrong way nor a chunk whose count of values is not its
group's rows. Its group is then read, and the reading raises what it
finds. A footer consistent with itself is trusted.

The decision by statistics is proved by Kani for every kind of clause,
every count of rows and of `NULL` rows, and every least and greatest
value, the emptiness of strings among them: a group ruled out holds no
row that passes. The decision by dictionaries is shown by tests: a
property test writes files of random values and strings, `NULL` among
them, of random groups and pages, with dictionaries and their fallback,
with statistics and without, under rewritten footers, and checks that
no group ruled out holds a row that passes.

Refused:

- **The footer's word that a string's bound is exact, required before
  `<> ''` is tried against the dictionary.** ClickBench's files do not
  give it, so every dictionary of theirs would be read.
- **The order of strings beyond emptiness.** ClickBench compares no
  string with another constant by its order or its prefix, and no bound
  rules out its `LIKE` of a part inside the string.
- **Rows skipped inside a group, by page indexes or Bloom filters.**
  ClickBench's files have neither.

## Parallel reading by groups of rows

The plan carries the files' paths and patterns. When a parallel scan
begins, the leader expands the patterns again, reads the footers through
the cache, and lays out in the plan's shared memory the list of files,
each file's identity, the rows of each group and the paths, with a
counter. Each participant takes the next group from the counter and
reads it as a serial scan reads its groups, so one file is divided as a
list of files is. A worker opens a file, and reads its footer, when it
first takes one of its groups, and checks the file it opens against the
identity the leader found. The workers are the core's for a scan of the
files' size. Over ClickBench's 14 GB file of 226 groups, four of its
queries that read one or two columns ran 5.6 to 6.2 times faster with
eight workers than with none.

When the parallel plan is rescanned, as the inner side of a nested loop
is, the leader resets the counter in the core's callback of a rescan of
the shared memory, and the list stays the one the scan found when it
began. The counters of `EXPLAIN` follow the list, one row a
participant. At that callback the workers of the scan that ended have
stored their counts and exited, so their counts join the leader's
before the rows are laid out anew, and `EXPLAIN` totals every scan.
Where the core gives no segment of shared memory, it puts the list in
the leader's private memory and frees it before the scan ends, so the
counts are summed when the plan shuts down.

Refused: **one file a participant**, which leaves ClickBench's one file
to one process.

## Files that change under a scan

Planning expands the patterns and maps each column the query reads to
each file. A scan does both again when it begins and reads the files as
they are then, so that a plan kept by a prepared statement reads a file
replaced since and refuses one that lost a column. The scan notes each
file's identity then. When it opens a file, serial or as a participant,
it compares `fstat` of the open file with that identity, so that no scan
reads a file under the footer of another version and no two
participants read two versions of a file.

A reader checks the file's size and time again after each read that
reached the file, before it uses the bytes, so that a file rewritten in
place under a running scan stops it rather than give rows of two
versions: a read that saw new bytes is followed by a check that sees
the new time, where the file system gives the write a new time. A
window whose pages were read before is not checked again. A file
replaced by a rename while a scan reads it is safe: the open file keeps
the old contents. A change that keeps the size and the time is not seen;
the guide's list of limits names such changes. A participant keeps open
only the file whose groups it reads.

## Costs and ANALYZE

The table's rows are its files' rows, from the footers, at planning; a
scan's rows follow PostgreSQL's selectivity of its clauses. Both paths
cost the bytes of the chunks of the columns they read, at
`seq_page_cost` a page of 8 kB, and every row of the files. The
wrapper's rows cost, for every row, the core's cost of a tuple and of
an operator for each column it forms, with the clauses the core checks.
The source's batches cost a quarter of the core's prices: for every
row, an operator for each column of the clauses it checks and for each
of those clauses; for the rows that pass, a tuple and an operator for
each column read. `TessFilter` above adds the clauses left. The quarter
is near the ratio of the serial times of a scan
of one or two columns with and without Tessera over ClickBench's file,
0.18 to 0.31. Both cost the targets PostgreSQL computes, on the rows
the relation returns, and a partial path a participant's share.

The share holds for scans, not for what is above them. A distinct
aggregate, which Tessera runs serially only, can then be chosen over
the core's parallel plan where that is faster; both give the same rows.

`ANALYZE` samples rows at random over all the rows of the files, each
set of positions as likely as another, finds them in their groups, and
reads only the rows it samples, by readers whose selection holds them.
Each row has the same chance, however the files divide the rows into
groups; a share fixed a group, refused, would leave out every other
group of a file of small groups. Reading only the rows sampled still
reads most pages of a wide file: the 30 000 rows of a sample fall in
most of them, so that `ANALYZE` of ClickBench's file decompresses most
of the file once. Since the table's rows are always the files' rows, the
counts of distinct values `ANALYZE` stored as a share of the rows grow
with files added after it.

## Memory, errors and cancel

Every object of the reader that the C side holds is freed by a callback
of the memory context that owns it, so that an error or a cancel frees
it too. A scan keeps three contexts, siblings so that a reset of one
does not delete the others: one for the file open and its columns
mapped, one for the readers of the group, one for a window's values.
The node keeps a fourth for a batch's arenas of strings, its
references to windows and the scratch of its checks under assertions,
reset at the next batch and at a rescan; a batch's arrays are the
scan's, in the query's memory. A fifth holds the values of the clauses'
sides that are known only at execution, reset at each rescan. The node
frees the clauses' tests with the file's context, since a test is made
for a file. The cache of footers is the one exception, bounded by its
count of files.

The node checks for a cancel between windows and between the calls
that read a window; the wrapper's rows check it between windows. The
suites stop a scan at a window through an injection point, a named
place in the code where a test module can act.

## What the part does not do

- **Remote files, writing, nested types, decimals, `INT96`, unsigned
  64-bit integers, codecs other than Snappy.** No query of ClickBench
  needs them.
- **Parquet's page indexes and Bloom filters.** ClickBench's files have
  neither.
- **`IMPORT FOREIGN SCHEMA`.** The columns, their units and their trust
  are a choice of whoever makes the table.
- **A string function on bytes, such as `REGEXP_REPLACE`.** It is the
  core's function, row by row, on the values made.

## Files

- `parquet/tessera_parquet.control`, `parquet/tessera_parquet--0.1.sql`:
  the extension and its wrapper
- `parquet/tessera_parquet.h`: the module's own header, the scan's state
- `parquet/tessera_parquet_reader.h`: the reader's C entry points
- `parquet/options.c`: the options and their privileges
- `parquet/files.c`: the patterns, the files and the mapping of columns,
  and the raising of the reader's statuses
- `parquet/fdw.c`: the wrapper's planning, paths, costs and callbacks,
  and the lines of EXPLAIN of both scans
- `parquet/scan.c`: the scan of files and groups both paths share, the
  wrapper's rows, the parallel list and its counters
- `parquet/node.c`: the source, its answers, and its node with its two
  passes, its batches and the groups it skips
- `parquet/analyze.c`: the sample of ANALYZE
- `parquet/Makefile`, `rust.mk`: the module and its link with the
  reader
- `crates/tessera-parquet/src/file.rs`: files, identities and the cache
  of footers
- `crates/tessera-parquet/src/column.rs`: the mapping of a column and
  the reader's view of the footer
- `crates/tessera-parquet/src/reader.rs`: windows read for a selection
- `crates/tessera-parquet/src/clause.rs`: the tests of clauses, and the
  decision of a group ruled out
- `crates/tessera-parquet/src/stats.rs`: a chunk's statistics under the
  format's rules of trust
- `crates/tessera-parquet/src/dictionary.rs`: a dictionary page read
  alone
- `crates/tessera-parquet/src/pages.rs`: the reader's row groups and
  page reader over the crate's lower entry
- `crates/tessera-parquet/src/snappy.rs`: the decoder of Snappy, and
  `crates/tessera-parquet/LICENSE-snappy`, the notice of the code it
  ports
- `crates/tessera-parquet/src/c.rs`: the C entry points, their statuses
  and their caught panics
- `crates/tessera-parquet/src/bytes.rs`: the bytes form's places and
  checks
- `crates/tessera-parquet/src/testing.rs`,
  `crates/tessera-parquet-testing`: the writer of the tests' files
- `docs/parquet.md`: the guide for the wrapper's users

## Tests

- `test/sql/parquet.sql` with `test/tessera_parquet_test.c`: the
  options and their refusals, every mapping of types, text from bytes,
  rows and batches against the same queries over heap tables of the
  same rows, the clauses checked and left, groups skipped and not,
  files replaced and damaged, parallel plans and their rescans,
  estimates, `ANALYZE` and `EXPLAIN`. The module's consumers of a batch
  ask the batches before the one they check, as values and in the
  bytes form, so that a row a batch does not make again shows, and a
  table whose first batch is short reads its columns into full batches
  after it. The test module writes every file
  the suite reads, so that no binary file is kept in the tree, and
  rewrites footers as older writers wrote them.
- `test/sql/parquet_cancel.sql`: a cancel within a group, and a file
  replaced between a parallel plan's leader and a worker, through
  injection points.
- `test/installed/Makefile`: the module built from copies against the
  installed headers and runtime library.
- `crates/tessera-parquet/src/*/tests.rs`: unit and property tests over
  files the crate's writer makes, among them a column marked as a
  string with bytes that are not UTF-8 read under a selection that
  leaves them out, the reader's dictionary pages against the crate's
  reader of the same chunk, and every strategy of the rule of
  comparisons. Miri runs the tests of the C entry points.
- `crates/tessera-parquet/src/clause/proofs.rs`,
  `crates/tessera-parquet/src/bytes/proofs.rs`,
  `crates/tessera-parquet/src/c/proofs.rs`,
  `crates/tessera-parquet/src/snappy/proofs.rs`: the proofs by Kani of
  the bounds of `IN`, of a group ruled out by its statistics, of the
  bytes form's arithmetic, and of a turn of the decoder's fast loop and
  its copy of a pattern.
- `crates/tessera-parquet/src/pages/tests.rs`: the reader's pages
  against the crate's over a file of its own, a header read by one read
  of the file, headers longer than the buffer read and skipped, and a
  page's size checked after reads, peeks and skips in every order.
- `crates/tessera-parquet/src/snappy/tests.rs`: the decoder against
  `snap`'s, on every kind of element, streams cut and given wrong
  lengths, and property tests, decoded between fences a stray write
  would change; and the fast loop at each edge of its guard.
- `crates/tessera-parquet/fuzz`: the fuzz target of the decoder against
  `snap`'s.
- `cargo clickbench decoders` (`tools/tessera-clickbench`): the
  decoder's instructions, cycles, branches and misses on a file's
  pages, against `snap`'s.
- `tools/tessera-crosscheck`: random queries over a table of the
  wrapper whose files hold a heap table's rows, with Tessera on, decided
  against the heap table with Tessera off.
