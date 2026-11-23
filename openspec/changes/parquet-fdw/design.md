## Context

`source-scan` lets a registered source read a foreign table in batches
for Tessera's nodes and check some of its clauses itself; its only
source is a test one. The crate `datum-arrow` gives the boundary of
types between PostgreSQL and Arrow (`openspec/specs/datum-arrow/`): read
plans checked once a column, the conversions of values into PostgreSQL's
representations, and the bounds of a comparison in a file's own unit.
ClickBench's Parquet files are the case the maintainer chose: 105
columns, about 500 000 rows a group of rows, Snappy, dictionary and
plain encodings, times as bare integers, strings as unmarked bytes
(proposal, "Why"). The columnar entries Tessera meets there run the
queries in DuckDB over all the machine's threads.

Two facts shape everything below. Tessera's batches speak PostgreSQL's
`Datum`: a consumer asks a batch for a column as arrays of values
(`get_datum_column`, `tessera/batch.h`), for the rows of a mask, as many
times as it needs and for any of the batch's rows, until it releases the
batch (`docs/batch.md`). And PostgreSQL divides a parallel scan among
its processes through the plan's shared memory, which a foreign scan and
a custom scan may use too.

A few words used below. A **group of rows** is Parquet's row group; a
**column chunk** is one column of one group, stored as **pages**, each
compressed apart; a chunk **encoded by a dictionary** stores its
distinct values once, in a dictionary page, and each row as an index
into it. The **footer** is the file's metadata at its end. To **make a
value** is to turn a file's value into the PostgreSQL value a consumer
gets. A **window** is a run of rows of one group that the node reads
together, many batches long. A group's **selection** is a bit for each
of its rows, set where the clauses the source checks pass.

## Goals / Non-Goals

**Goals:**

- Read ClickBench's files in place, with every row right, as a foreign
  table any PostgreSQL client can query.
- Make a value only when a consumer asks for it: no column a query does
  not read, no row a clause rejects, no row a top-N sort passes over.
- Check the common clauses on the file's stored values, before any value
  is made, and skip groups of rows that cannot pass.
- Divide one file among the processes of a parallel plan.
- Measure each stage against pg_duckdb over the same files on the same
  machine.

**Non-Goals:**

- Grouping by a string on the file's bytes, one value a group: later,
  after the measurement's fourth stage gives its base.
- Writing Parquet, remote files, nested types, decimals, `INT96`.
- ClickBench's submission itself.

## The whole

```
 a query over a Parquet foreign table t

 planning   GetForeignRelSize ── footers (cached by path, size,
                                 time, device and inode)
            datum-arrow: a read plan for each column the query reads,
                     or the query refused
            GetForeignPaths ──► ForeignPath serial, ForeignPath partial
                                     │
            scan planner (source-scan): offers each to the sources
                                     │
            tessera_parquet's source: answers for each clause,
            returns TessParquetScan's path ──► TessFilter if clauses left
                                     │
 execution  TessParquetScan          ▼
            ┌────────────────────────────────────────────────────────┐
            │ take a group of rows (a shared counter in parallel)    │
            │ skip it by its NULL counts, its least and greatest     │
            │   values or its dictionaries                           │
            │ read the checked clauses' columns forward, window by   │
            │   window, and check the clauses on their stored values │
            │   → the group's selection                              │
            │ publish batches of 64 selected rows                    │
            │ a column asked for a batch: its window read for the    │
            │   selection, by a reader that goes forward; values     │
            │   made for the rows asked, kept until the batch is     │
            │   released                                             │
            └────────────────────────────────────────────────────────┘
                                     │
            crates/tessera-parquet: footers, windows of a column read
            for a selection (parquet crate), clauses on stored values
```

With Tessera off, or where the planner prefers it, the same reader
serves the wrapper's `ForeignScan` row by row, and the core checks the
clauses.

## Decisions

### A module of the repository, in C and Rust

The extension `tessera_parquet` holds the wrapper, its source and its
node, in C, since PostgreSQL's interfaces for a wrapper, a custom scan
and errors are C. The files are read by a Rust crate of the workspace,
`crates/tessera-parquet`, linked into the module as a static library,
with its entry points in a header of the module; it uses `datum-arrow`
for every conversion of a value, and `tessera-kernels` for the pattern
of LIKE alone (below). The module uses the public C API of Tessera alone
(the contract of `source-scan`), so that it builds against the installed
headers as a module of another extension would.

Refused:

- **The reader in C over Apache Arrow's C++ library.** A large C++
  dependency at build time, from packages Tessera does not ship.
- **A reader of our own.** Parquet's encodings, levels and codecs are a
  format of their own; the `parquet` crate reads them and is used by
  hundreds of crates (proposal, "Impact"). One exception: the values of
  a dictionary page, plain values whether its header says `PLAIN`, as
  the crate's writer writes it, or `PLAIN_DICTIONARY`, as ClickBench's
  writer does, which the skip of a group by its dictionary reads alone
  (below), are decoded by the reader's own code; a dictionary page of
  any other encoding leaves the dictionary untried, without an error,
  since the crate's decoders of values sit behind its feature
  `experimental`; the reader's tests compare that code with the crate's
  reader of the same chunk.
- **The module as a part of `tessera_nodes`.** A source is a module of
  another extension by the contract; the wrapper also brings the
  reader's dependencies, which the nodes do not need.

### The reader: windows of a column, for a selection

The `parquet` crate reads a column of a group of rows into Arrow arrays
through its record batch reader (`ParquetRecordBatchReaderBuilder`,
made from a footer already read, with the group, the column, a
selection of the rows to read and the size of its arrays), under the
policy of selectors (`RowSelectionPolicy::Selectors`): the rows it
does not select it skips, a whole page without decompressing it where
the skip covers the page, and otherwise inside the decompressed page.
The crate's default may turn a selection of short runs into a mask,
which decodes every row and filters after; the measurement of stage 2
tries both. A
chunk of strings encoded by a dictionary comes back as Arrow's
dictionary array, the dictionary and each row's index, when the
reader's schema asks for it. The crate's features are `arrow` and
`snap`, no more.

The reader is given the footer with two changes. The marks are taken off
the columns the query reads as text or bytes, off every column the query
does not read, and off every group, such as a list or a map, of which
the query reads no column, which then converts as a plain group, so that
the crate reads every `BYTE_ARRAY` as bytes and checks none of them, and
a column of a mark the crate cannot describe fails only a query that
reads it: the crate checks UTF-8 itself, in every page and dictionary it
decodes, for a column marked as a string, which would fail on rows a
clause rejects and on `'trust'`. And the schema of Apache Arrow a writer
may keep in the file's metadata is ignored, so that a column's Arrow
type comes from its physical type and mark alone. The schema the reader
gives the crate asks for every `BYTE_ARRAY` as Arrow's `LargeBinary`, or
a dictionary of it where the chunk is encoded by one: its offsets are
64-bit, since the strings of one array may pass the 2 GB that the 32-bit
offsets of `Binary` hold even when each value fits PostgreSQL, and the
crate would fail the array before `datum-arrow` sees a value.

A writer whose dictionary grows past its limit falls back to plain pages
for the rest of the chunk, as most chunks of the long strings of
ClickBench's files do. Read as a dictionary, such a chunk costs twice:
the crate builds a dictionary of each array of its plain pages, hashing
every value, and a window over two arrays of different dictionaries
merges them, hashing again. So a chunk is read as a dictionary only
where it has a dictionary page and its footer names no data page
encoded otherwise; a footer that does not say how its data pages are
encoded leaves the chunk read through its dictionary, which is right
for a chunk that does not fall back.

The node reads a group in two passes. First the columns of the clauses
it checks, forward over every row of the group, window by window, the
clauses checked on each window's stored values and the values then
dropped; the group's selection is whole before its first batch is
published. Then every other column is read only when a consumer first
asks for it in one of its batches, for the window's selected rows. A
column asked window after window, as a scan in full or a grouping asks
it, keeps one reader that goes forward over the group with its
selection, so that each page is decompressed and the dictionary decoded
once a group; its arrays hold a fixed count of selected rows, so it may
read into the next window, and keeps what it read past the window for
the next ask. A window after a gap, past windows no consumer asked for,
as when a clause above the scan passes no row of a window, builds the
forward reader anew from that window, which skips the pages of the gap;
the windows after it are forward again, read once each. The new reader
decodes the dictionary again, walks the headers of the pages before the
window and decompresses the page under its start again, so a clause
above the scan that passes rows in scattered windows pays that for each
window asked; the measurement of stage 2 weighs it against reading a
short gap and dropping its rows. An ask behind the forward reader, a
window already passed, as a top-N sort asks for the rows it kept, builds
a reader for that window and the rows asked: it decodes the dictionary
again and walks the headers of the pages before the window, but
decompresses only the pages that hold the rows. A window no consumer
asks a column for is never read for it. A clause's column a consumer
asks for is read again, for the selected rows alone.

Refused:

- **One record batch reader over all the columns asked.** It decodes
  every projected column of every row before the consumer knows which
  rows it keeps: a top-N sort over `SELECT *` would decode the 105
  columns of every row.
- **A whole column chunk decoded at its first use.** A top-N sort that
  takes a row from many groups would decode whole chunks of every column
  for a few rows.
- **The crate's array readers, which read and skip forward.** They sit
  behind its feature `experimental`, which brings the crates of the
  `variant` type.
- **A reader for the rows asked after a gap.** Each batch of the windows
  after it would build one and decompress its page again, so that a
  clause above the scan that passes no row of one early window would
  leave the rest of the group read 64 rows at a time. Nor is the gap
  read and dropped: that decodes what no one asked for, where a new
  forward reader skips it.
- **A reader for each column and window.** Each reader starts at the
  chunk's start: over a scan in full it would decode the dictionary and
  decompress the page under each window boundary once a window.
- **Marked strings read as Arrow's `Utf8`.** The crate checks UTF-8 in
  every value it decodes, so a value no consumer asks for, or a trusted
  column, would fail or pay the check; and `datum-arrow` would see a
  type other than the one of an unmarked column.

### Values made, and kept for the batch

The node's values are made in Rust, by the reader's entry point, through
the conversions of `datum-arrow`, into the arrays and the arena the C
node allocates: the reader gives the sizes a batch's column needs, the C
side allocates them, and the reader fills them, the `Datum` words and
each string's header and bytes, so that no call into C allocates under a
Rust frame. An integer, a date or a time is a few arithmetic steps from
the stored integer into a `Datum`, a string a `text` whose header is the
short one of one byte where it fits, up to 126 bytes, and the aligned
one of four bytes otherwise, in one arena a column and a batch, so that
a batch's strings cost one allocation, not one a row. A consumer may ask
a batch for a column several times, for rows that do not nest and in any
order: the node keeps the values it made and a bit for each row made, as
the heap batch does, and makes only the rows not made yet.

A later ask converts straight into the batch's arrays, and what the
earlier asks made stays, by what `datum-arrow`'s read promises: for a
plain column of integers, floating-point numbers, days, times or
booleans it may write the place of a row not asked, but only with that
row's true value where the value converts, so a value made before is
written again with itself, and the place of a row whose value does not
convert, or that is `NULL`, was never made; for strings, bytes and
dictionaries it writes the places of the rows asked alone. Its mask of
`NULL` rows is whole words, with the bits of rows not asked clear, so
the reader converts into a mask of its own for the call and copies into
the batch's only the bits of the rows asked. The arena and the values go
when the batch is released. `Values Decoded` in `EXPLAIN ANALYZE` counts
the values made. The column arrays of a batch are zeroed when the batch
is made, since the batch's contract wants every row below its count
written, and the node makes only the rows asked. The node's batches
leave `values_stored` of `tessera/batch.h` false: a read of them can
fail on a value that is not damaged, such as text that is not UTF-8, so
a parent that gathers sparse batches must not ask every column of every
row before it drops rows; and the node's batches are full already, since
a window publishes its selected rows 64 at a time.

### Times and dates in the mapping of columns

The column option `unit` says what a bare integer counts, so that
`EventTime` comes up as a `timestamp` and `EventDate` as a `date`; it
becomes the unit of the column's read plan in `datum-arrow`. pg_duckdb's
entry reads them through a view of expressions over `interval`; over
such a view a clause `EventDate BETWEEN …` would be an expression
Tessera's kernels do not take, and every row would go through the core's
evaluation. In the mapping, the clause is a comparison of a column with
a constant: the source checks it on the stored integers, and the kernels
take what is left.

### Bytes as text: checked when they become values

The files' strings carry no mark, and checking every row's `URL` and
`Title` would cost on every query. A column marked as a string is read
as bytes too (the reader above), so that one rule holds for both. The
column option `utf8` decides, through the column's read plan: `'check'`
checks each value made, and only those, so that rows a clause rejects,
columns no one reads and rows a top-N sort passes over cost nothing;
`'trust'` checks nothing, on the promise of whoever made the table. The
wrapper's rows make every value of every column a query reads, so they
check more than the source's batches; the two give the same rows when no
value fails. The clauses the source checks on bytes, equality and
`LIKE`, read the bytes as they are: under a deterministic collation the
core's `LIKE` and equality on UTF-8 compare bytes too.

### The clauses the source checks, on stored values

The source answers exactly for the clauses the spec lists and leaves the
rest. The reader checks them on the window's Arrow arrays, before any
value is made, into the window's selection:

- a comparison of an integer, a date or a time, through the test that
  `datum-arrow` makes once from the constant in the file's unit, a
  constant of any integer type for an integer column, exact over every
  stored value, so that a stored value out of PostgreSQL's range
  compares as any other and fails no row by itself;
- `IN` the same way, against a sorted list of bounds;
- a string's equality with a constant by its length first and its bytes
  after; `<> ''` by its length alone; on a dictionary array, a clause is
  checked once a dictionary value and the rows take their index's
  answer. The answers are kept for the chunk's dictionary, which every
  window of the chunk shares: each window is an array of its own over
  the same buffers, so the dictionary is known by the addresses of its
  offsets and bytes, and the test holds its array while it keeps the
  answers, so that no other dictionary can take those addresses;
- `LIKE` with fixed parts and `%`, by the pattern of the kernels' own
  `LIKE`, `Like` of `tessera-kernels`, so that the source and
  `TessFilter` match a string by one rule: each part found in order, the
  first anchored at the start and the last at the end unless a `%`
  stands there, each part at or after the end of the one before it, so
  that `'ab%ba'` rejects `'aba'`.

The node takes a clause on a string by the function of its operator,
`texteq`, `textne`, `textlike` or `textnlike`, under a collation that is
deterministic; a `varchar` column comes through the relabeling to `text`
its operators take. A pattern of `LIKE` must be a constant, which the
reader takes or refuses at planning: one with `_`, with the escape
character `\` or with more than 31 `%` signs, the 32 parts the kernels'
pattern holds, is left to `TessFilter`, as is `ILIKE`.

A `timestamp` compared with a `timestamptz` is left to `TessFilter`:
PostgreSQL turns each row's time into the session's zone, which no
single bound can stand for across a change of daylight saving. A clause
on a parameter or a subquery's value takes the value through
PostgreSQL's evaluation of expressions when the node is first asked for
rows, as `source-scan` asks. The checks raise no error and write no
message. Refused: the answers `TESS_CLAUSE_PRUNE_ONLY` for these
clauses, which would have `TessFilter` check them again on values the
source made only for it.

Which comparison a clause asks is found in two steps. The node asks
PostgreSQL's catalog for the operator's strategy in the column type's
B-tree family, the number that says "less", "equal" and so on, and for
`<>` the strategy of its negator. The reader then turns the strategy,
the order of the operands and the pair of types into a comparison of
the column with the side, or refuses the clause. The rule is small and
closed, so a Rust test runs it for every strategy, negator, order and
pair of a list of types, and checks each answer against the operator's
meaning on values. Kept in C, it would be judged by the suites alone,
since only Rust tests judge a mutant.

The rest is proved or compared with PostgreSQL. A property test compares
each test of strings, over plain and dictionary arrays and their slices,
with a match of the pattern one byte at a time. Kani, a checker that
proves a property of a Rust function for every input, proves in
`datum-arrow` each comparison's bound exact, for seconds and
milliseconds past the division of the constant by the unit, and an
equality's test one bound or none. In the reader it proves the bounds of
`IN` the union of its values' tests of equality, for lists of two values
over plans of integers, days and microseconds; it asks of a test no more
than that one bound or none, so it holds for every unit. Their sort and
the binary search over them are the standard library's, whose sort takes
Kani past the limit of a harness. The property tests compare the whole,
over every kind of array, with PostgreSQL's answer.

### Groups of rows skipped

Before a group is read, each clause the source checks is tried against
what the footer says of its column's chunk, in the order of the spec:
the count of `NULL` values, which alone decides `IS NULL` and `IS NOT
NULL`, a missing count taken as some (every other clause the source
checks is strict and passes no `NULL`); then, for integers, dates and
times, the least and greatest values where Parquet's rules let a reader
trust them (the fields `min_value` and `max_value` only under a column
order of the column's type, since without one their order is undefined;
the older `min` and `max`, which the crate's statistics mark as
deprecated, only for signed values, since a writer always sorted them as
signed), through the same tests of `datum-arrow` the rows use; then,
where these do not rule the group out, the dictionary: for a column
without least and greatest values, and for a value between them that no
row holds. A chunk is tried against its dictionary only where the
footer's statistics of encodings say that every data page is encoded
through it, `PLAIN_DICTIONARY` or `RLE_DICTIONARY`, since a writer may
fall back to plain pages when the dictionary grows; the dictionary page
alone is read, through the crate's page reader, and the group skipped
when no value of it passes. Where the footer does not say how the pages
are encoded, the dictionary is not tried. The reader takes the crate's
default form of these statistics, a mask of the encodings of the data
pages alone (`page_encoding_stats_mask`), and tries a chunk that has a
dictionary page and whose mask holds no encoding but `PLAIN_DICTIONARY`
and `RLE_DICTIONARY`, either or both; a list of every page, which the
crate builds only on request, would cost a vector for each chunk of
every file. ClickBench's files hold these statistics on every chunk; 198
of the 210 chunks of `hits_0.parquet` are encoded through a dictionary
in every data page, and their dictionary pages are headed
`PLAIN_DICTIONARY`. Of the least and greatest values of strings only
their emptiness is tried, for `= ''` and `<> ''`, before the dictionary,
and only from the fields `min_value` and `max_value` under the column
orders, as for integers: older writers wrote wrong least and greatest
values of bytes in the fields `min` and `max`. Every order of bytes puts
the empty string first, so a least value that is not empty, even one a
writer cut short, shows that no row is empty, and a greatest value that
is empty shows that every row is. A greatest value that is not empty
leaves `<> ''` untried against the dictionary: the format lets a writer
give a greatest value past every row, so it does not prove that a row
passes, but a group it does not skip is read and its rows checked, and
none comes out wrong; requiring the footer's word that the value is
exact would read every dictionary of ClickBench's files, which do not
say it. ClickBench's `SearchPhrase <> ''` and `MobilePhoneModel <> ''`
are decided so in every group, where the dictionary would be read and
decompressed, 468 614 bytes of `SearchPhrase` in a group, to skip
nothing. The order of the least and greatest values of strings is not
tried otherwise: ClickBench compares no string by `=` with another
constant or by a prefix, and its `LIKE` of a part inside the string no
bound can rule out.

The reader decides and the node asks. When a group starts, the node asks
for each clause the source checks whether the footer rules the group
out: first by the statistics of every clause's column, which cost no
read, and then by the dictionaries, so that a dictionary page is read
only where no statistics decide. A group ruled out is counted as
skipped, not read. A footer that contradicts itself rules nothing out:
neither a range turned the wrong way nor a chunk whose count of values
is not its group's rows, whose group is then read, and the reading
raises what it finds. A footer consistent with itself is trusted, as the
format's statistics are. The decision is in Rust, where Kani and the
mutants judge it. Kani proves the decision by statistics for every kind
of clause, every count of rows and of `NULL` rows and every least and
greatest value, the emptiness of strings among them: a group ruled out
holds no row that passes, and `IN` of
up to three bounds is ruled out exactly where a range the right way
round holds none of them. The decision by dictionaries is shown by
tests. A property test writes files of random values, `NULL` among them,
signed and unsigned, of random groups and pages, with dictionaries and
their fallback, with statistics and without, under rewritten footers,
and checks that no group ruled out holds a row that passes; another does
the same over files of random strings, empty ones among them, by each
clause on strings. Two clauses on one column read its dictionary twice;
ClickBench asks no such pair that the statistics leave undecided.

### Parallel reading by groups of rows

The plan carries the files' paths and patterns. When a scan begins, the
leader expands the patterns again, reads the footers, through the cache,
and puts the list of files and groups and each file's size, time of
modification, device and inode in the plan's shared memory, with a
counter; each participant takes the next group from the counter, so that
one file is divided as a list of files is, and reads its groups as a
serial scan reads them. A scan, serial or a participant of a parallel
one, checks each file it opens, by `fstat` of the open file, against the
size, time, device and inode found when the scan began, and raises
`55000` on a difference, so that no scan reads a file under the footer
of another version and no two participants read two versions of a file;
the suite replaces a file between the leader and a worker through an
injection point before the worker opens it. When the parallel plan is
rescanned, as the inner side of a nested loop is, the leader resets the
counter in the core's callback of a rescan of the shared memory
(`ReInitializeDSMForeignScan`, `ReInitializeDSMCustomScan`), and the
list of files stays the one the scan found when it began. The counters
of `EXPLAIN` lie after the list, one row a participant, the runtime's.
At that callback the workers of the scan that ended have stored their
rows and exited, and the leader stores its own only when the plan shuts
down: their counts join the leader's before the rows are laid out anew,
so that `EXPLAIN` totals every scan, where Tessera's batch nodes keep
the last scan's workers alone. Where the core finds no segment of shared
memory, it puts the list in the leader's private memory and frees it
before the scan ends, so the counters are summed when the plan shuts
down. The workers are the core's for a scan of the files' size.
Refused: one file a participant, which leaves ClickBench's one file to
one process.

### The footers, read once, and files that change

Planning reads each file's footer for the rows, the groups and their
statistics, and keeps it in the backend's memory by its path, size, time
of modification, device and inode, so that a query does not read the
footers of a hundred files again; a worker, a new process each query,
reads a file's footer only when it first takes a group of that file,
from the leader's list, and a file replaced by a rename is read anew
even with the same size and time. The cache holds the footers of at most
a thousand files, the least recently used dropped first. A scan reads
the files as they are when it begins, through the same cache, and checks
its columns against them again, so that a plan kept by a prepared
statement reads a file that was replaced since. A participant keeps open
only the file whose groups it reads, its descriptor counted against the
backend's through `AcquireExternalFD` and `ReleaseExternalFD`, since the
reader opens it outside PostgreSQL's own files, and it checks the file's
size and time after each read, before it uses the bytes, so that a file
rewritten in place under a running scan raises `55000` rather than
giving rows of two versions: a read that saw new bytes is followed by a
check that sees the new time, where the file system gives the write a
new time.

### Costs and ANALYZE

The paths cost the bytes of the column chunks they read, at
`seq_page_cost` a page of 8 kB, the compressed sizes of the chunks the
footers give, and their rows, every row of the files: the wrapper's rows
at the core's cost of a tuple and of an operator for each column it
forms, with the clauses the core checks on each, the source's batches at
a share of that cost, a quarter, near the ratio of the serial times of a
scan of one or two columns with and without Tessera: the columns of the
clauses it checks for every row, with the clauses, and the other columns
for the rows that pass, as many as the clauses' selectivity gives, the
clauses it leaves to `TessFilter` above, which adds them. Both cost the
targets PostgreSQL computes at their whole cost, on the rows the
relation returns. A partial path costs a participant's share of both.
The table's rows are its files' rows, so that the distinct values
`ANALYZE` stored as a share of the rows grow with files added after it.
`ANALYZE` samples rows at random over all the rows of the files, each
set of positions as likely as another, finds them in their groups, and
reads only the rows it samples, by readers whose selection holds them; a
share fixed a group would leave out every other group of a file of small
groups. For 30 000 rows of a wide file the rows still fall in most
pages: `ANALYZE` of `hits.parquet` decompresses most of the file once.
The core keeps their statistics; ClickBench's groupings and joins are
then sized by them.

### Errors, panics, memory and cancel

Every call into the crate returns a status; the C side raises the error
after the call has returned, so that no Rust frame is on the stack when
PostgreSQL's error unwinds. Every entry point catches a panic, as
`tessera-capi`'s do: a panic while the `parquet` crate decodes a file is
reported as a file it cannot decode, any other as an internal error. The
crate's objects belong to the node's memory context through a callback
of its reset, so that an error frees them. The node checks for a cancel
between windows and between the calls that read a window.

### Tests with files the suite writes

The suite `test/sql/parquet.sql` writes its files with a test module
over the `parquet` crate's writer: the types and marks of the spec,
`NULL` values, several groups of rows, dictionary and plain encodings
and pages that fall back from one to the other, least and greatest
values written and absent, and an Arrow schema in the metadata that
names other types. Bytes that are not UTF-8 in a column marked as a
string are written through the crate's writer of columns, which takes
bytes as they are. The test module rewrites a footer to give the least
and greatest values of the old rule, the deprecated fields without
column orders, which the crate's writer never writes, on a column of
plain pages, and to drop the statistics of encodings and the counts of
`NULL` values; it reads the footer with the statistics of encodings
whole, since the crate writes back only those it read so. The crate's
writer always writes column orders, so a footer without them, as older
writers wrote it, is made by cutting them from the footer it wrote:
they are its last field, of a shape the module checks byte by byte and
refuses otherwise. The old fields under column orders and the newer
fields without them are written the same way. No binary file
is kept in the tree. Every query is compared with the same query over a
heap table of the same rows.

### The measuring tool

`tools/tessera-clickbench` (`cargo clickbench`), as `cargo tpch` is for
TPC-H: it makes a cluster, a foreign table over ClickBench's files (one
file or a list), and runs the 43 queries in three modes, Tessera on,
Tessera off over the wrapper's rows, and pg_duckdb over a view of
`read_parquet` where pg_duckdb is installed, timing each three times as
ClickBench does and reporting the times and their geometric means. It
checks the answers of the modes against each other by companion queries:
a query whose `LIMIT` takes groups or rows in no total order, such as
Q18's ten groups and Q24's ten rows by `EventTime`, is checked by the
count of its rows, by its rows being contained, with their repeats, in
the full result of the query without the `LIMIT`, as `cargo tpch`'s
comparison keeps the repeats of rows, and by its sort keys being equal
across the modes. A run is one stage in one mode, under a deadline of
five minutes as `docs/measuring.md` asks: the tool sets a statement
timeout and reports a query that does not finish as such, untimed. The
queries are ClickBench's PostgreSQL text, in `bench/clickbench/`.

### Decisions of the maintainer

On 2026-11-20, after the analysis of ClickBench:

- Tessera enters ClickBench reading the benchmark's Parquet files in
  place, through a Parquet foreign data wrapper, a module of this
  repository, the first source of `source-scan`; the source filters.

On 2026-11-21, with `source-scan` open:

- `parquet-fdw` is built on `source-scan`'s branch, after `types-crate`,
  in a pull request of its own, before `source-scan` is merged.
- The types first: `types-crate` implemented and merged into `main`
  before this change's code, and this change taken up once its spec is
  reviewed.
- The least and greatest values of a string tried for their emptiness
  alone, for `= ''` and `<> ''`, before its dictionary.
- A query whose targets or clauses read a whole row of the table, such
  as `SELECT t FROM t` or `row_to_json(t)`, or a system column such as
  `tableoid`, is read by the wrapper's scan, not by the source: the
  runtime's projection computes columns alone, as the heap scan leaves
  such a query to the core's scan, and no query of ClickBench reads one.
  Teaching the projection a whole row and `tableoid` is the roadmap's
  entry `projection-whole-row`.
- PostgreSQL's `Datum` is the common language of the batches; Tessera
  counts on no packages of PostgreSQL's distributions.
- The order of the work: the parallel handout of groups of rows, then
  the 17 queries that read no string, then grouping by strings; each
  stage measured with Tessera on and off against pg_duckdb over the same
  files on the same machine.
- Times and dates in the mapping of columns, not through views; the
  check of UTF-8 decided by a column option and run only on the values
  made; a top-N sort reading its other columns only for the rows it
  keeps.
- A Parquet entry of ClickBench, like pg_duckdb's, is marked `stateless`
  and stays in the category Challenger.

On 2026-11-22, in the review of the clauses on strings:

- The reader takes the pattern of LIKE from `tessera-kernels`, a crate
  not meant for use outside Tessera, so that the source and `TessFilter`
  match a string by one rule. Was: the rule "Only the public C API"
  promised a module built against the installed headers and its own
  reader alone. Will be: the rule names that crate as its one exception,
  until the pattern moves to a crate meant for use outside Tessera, an
  entry of the roadmap. Why: a copy of the pattern in the reader would
  keep one rule in two places; the move is work of its own.

## Was and will be

- **Foreign tables in batches.** Was: the test source alone. Will be:
  Parquet files through `tessera_parquet`.
- **The workspace.** Was: `datum-arrow` and its `arrow-schema`. Will be:
  also the crate `tessera-parquet` and its dependency `parquet`; it
  takes `tessera-kernels` for the pattern of LIKE.
- **Measuring ClickBench.** Was: scripts of one run in
  `target/bench-runs`. Will be: `cargo clickbench`.
- **The nodes, the runtime and the bridge.** Was and will be the same.

## What we do not do

- **Grouping by a string on the file's bytes.** Fifteen queries group by
  a string, two of them all the rows by `URL`; a value a row before the
  hash costs what reading saved. Hashing the file's bytes and making one
  value a group needs a form of column other than `Datum` in the batch
  and a grouping that reads it, another change, which the measurement's
  fourth stage gives its base.
- **`REGEXP_REPLACE` on bytes** (Q29): the core's function, row by row.
- **Remote files, writing, nested types, decimals, `INT96`, unsigned
  64-bit integers, codecs other than Snappy.** No query of ClickBench
  needs them.
- **Parquet's page indexes and Bloom filters.** ClickBench's files have
  neither.

## Risks / Trade-offs

- [The windows are too large or too small] → too large reads rows of a
  window a consumer asks once; too small builds a reader for each
  window a top-N sort asks out of order, each decoding the dictionary
  and walking the page headers before it again; the measurement of
  `SELECT *` under a top-N sort sets the size.
- [The footer's string marks taken off the reader's copy] → rests on
  the crate taking its columns' types from the footer it is given; the
  reader's tests read a marked column with bytes that are not UTF-8
  under a clause that rejects them.
- [A trusted `utf8` column holds bytes that are not UTF-8] → PostgreSQL
  may show them or fail later in another function; the option says it is
  the promise of a role that may read the server's files, the only one
  that may set it.
- [A file is rewritten in place with the same size and its time kept,
  as `cp -p` or `rsync -t` may, or written through a shared `mmap`,
  whose time changes only at `msync` or writeback, or over NFS, whose
  `fstat` answers from a cache of attributes for seconds, or within one
  tick of a coarse clock, as Linux kernels before 6.13 give a file's
  time, after the write the scan saw] → undetected
  by its size and time; the rows may then be wrong. A file replaced by
  a rename while a scan reads it is safe for a scan that opened it: the
  open file keeps the old contents, and a scan that opens the new one
  finds another inode and raises `55000`.
- [The planner takes a source's path as one whose values are stored] →
  the batch planner's estimate of a lookup join takes the outer side's
  batches as gathered, which the runtime does not do for batches whose
  `values_stored` is false; the join's cost is then wrong, not its rows.
  The roadmap's "Not placed" holds it.
- [The cost model misprices the source against the wrapper's rows] →
  plans move between the two, and both give the same rows. The share of
  a quarter holds for scans; a distinct aggregate, which Tessera runs
  serially only, can be chosen over the core's parallel plan where that
  is faster, which the roadmap's "Not placed" holds.
- [An error differs between the two paths] → the wrapper's rows make
  every value read and may raise an error of a value the source's
  batches never make, as Tessera's batch nodes may already
  (`docs/limitations.md`).

## Measurement plan

On the M5 Pro, idle and on mains power as `docs/measuring.md` asks, with
`cargo clickbench` over the ten files of the earlier run
(`target/clickbench-data`, 10 M rows) and then over ClickBench's one
file `hits.parquet` (100 M rows, 14 GB), each stage's queries with
Tessera on, Tessera off and pg_duckdb, three tries, hot, in the order
the maintainer set:

1. **Parallel groups of rows**, after the source's batches and the
   parallel handout: Q1 to Q7, by 0, 2, 4 and 8 workers. Tessera on must
   beat Tessera off on each, and scale with the workers on the one file.
2. **The 17 queries that read no string**, after the clauses on
   integers, dates and times and the skipped groups.
3. **The 18 queries that filter on a string**, after the clauses on
   strings: a stage of this change's own, between the maintainer's
   second and third.
4. **The 15 queries that group by a string**, as they are: the base of
   the grouping on the file's bytes.

Each stage is done when Tessera on beats Tessera off on every query of
it; its geometric mean against pg_duckdb's is recorded. A Parquet entry
of ClickBench is worth submitting when the 43 queries' hot geometric
mean is within twice pg_duckdb's on the same machine; otherwise the
Parquet times are published only beside the heap result on a machine
that holds the table in memory.

## Open Questions

- Where ClickBench's entries are run for submission, c6a.4xlarge, and
  who runs it.
