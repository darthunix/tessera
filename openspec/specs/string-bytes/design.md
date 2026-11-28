# string-bytes: how it is built

A batch of Tessera gives a string column to the nodes that read it as
PostgreSQL values, one `Datum` a row, and a `text` value is a header
and a copy of the string's bytes. Many readers need only the bytes: a
grouping hashes a key's bytes, a kernel counts a string's characters, a
sort makes its word from a key's first bytes. The **bytes form** is a
second way to read a string column: each row's bytes where the
producer of the batch holds them, with no value made. This page
explains the idea and how it is built. The exact layouts, rules and
errors are in the capability's spec, [spec.md](spec.md), next to this
page, and the rules of the part's build in its contract,
[contract.md](contract.md).

## Background

A **batch** is a group of up to 64 rows of a node, kept by column, with
a mask of the rows still taken. Its **producer** is the node or the
source that made it; its **consumers** are the nodes and kernels that
read it. A batch gives a column through its operations, a table of
functions (`TessBatchOps` in `tessera/batch.h`): until this form there
was one way to read a column, `get_datum_column`, into a
`TessDatumColumn`, a `Datum` and a `NULL` flag a row. That structure's
size is the stride of every array of columns the kernels read, and a
Rust mirror of it checks its layout.

The **Parquet source** reads a Parquet file a window of 1 024 rows of a
column at a time, kept as an Arrow array: a plain array of strings, or
a **dictionary**, an array of the distinct values and an index a row.
Before this form, it made a `text` value for each row a consumer asked:
it checked the bytes, as the column's rule asks, then copied them
behind a header into an arena of the batch.

Three consumers read strings by their bytes. **TessAgg**, the grouping
node, numbers a string key in a **key dictionary** of its own: it
hashes the value's bytes, compares bytes, and copies a value once, when
it first appears; its table of groups holds the number. The **string
kernels**, the batch functions of `length`, `LIKE`, `=` and their
kind, read their rows in Rust through a trait of string sources.
**TessSort** sorts by a word a row, a fixed-size prefix of the key that
orders most rows, and compares ties by the type's function.

## The problem

Over ClickBench's Parquet file, Tessera on and serial took four to
five times the instructions of pg_duckdb on the queries that group, sort
or measure strings: 91.9 billion against 23.0 for Q13, `GROUP BY
SearchPhrase`; 74.6 against 20.1 for Q26, `ORDER BY SearchPhrase LIMIT
10`; 351.2 against 75.4 for Q28, `avg(length(URL))` by `CounterID`.
Profiles put a quarter to a third of their time on making the values:
copying the bytes, checking UTF-8, writing headers, for consumers that
then read the bytes back. 15 of ClickBench's 43 queries group by a
string, and Q34 and Q35 group all 100 million rows by `URL`. The heap
pays nothing for its values, which point into its pages; a source that
decodes a file pays for every row.

## Goals and what they cost

- **A string's bytes reach the consumers that need only bytes, with no
  value made a row**, through one optional form of the batch contract.
  It costs a second way to read a column, in every consumer that takes
  it, and a lifetime of pointers into the producer's memory.
- **Every result stays the one the `Datum` path gives**, the errors of
  the check of UTF-8 among them; a batch without the form is read as
  before. It costs tests that compare the two forms on every consumer.
- **The rule of the check keeps one owner, the producer of the form.**
  It costs the producer a check that is cheap only where it can be made
  once for many rows: once a dictionary, once a window.

## The whole in one picture

```
 TessParquetScan                               consumers
 ───────────────                               ─────────
 window of 1 024 rows  ── get_bytes_column ──▶  TessAgg: key number by
 (Arrow: offsets and       per row: pointer,     bytes, or by index once
  bytes, or a dictionary   length, index;        a dictionary; a value
  and indexes)             a dictionary's id     made once a group
        │                                       kernels: length, LIKE,
        │ the check of the column's rule,         = … read the bytes
        │ once a dictionary value or a window   TessSort: a word of the
        │                                         first 8 bytes; values
        └── get_datum_column ──▶ every other      made for kept rows
            consumer, and the rows that need      alone
            a value (output, heap)
```

A consumer that can read bytes asks for them first. A `false` answer,
or a batch without the operation, sends it to `get_datum_column`, as
before. A consumer that needs values for some rows, a row that beats
the worst of a top-N heap, asks `get_datum_column` for those rows
alone, and the source makes values for the rows asked only. TessAgg
asks the source for no value: it makes a group's value from its bytes
when the value enters its dictionary, and the value of a row it writes
to a spill file the same way.

## An operation of its own

The form is a structure of its own, `TessBytesColumn`, filled by an
optional operation appended after `release` in `TessBatchOps`,
`get_bytes_column`. A consumer finds the operation by the size the
table declares, so a producer built before the form gives a table that
ends at `release`, and its consumers read `Datum`s. The spec draws the
structure and says what each field holds.

Refused:

- **Fields at the end of `TessDatumColumn`**, as a numeric column's
  decimals are. Its size is the stride of every array of columns the
  kernels read: 32 bytes more on every column of every array, for a
  form few columns have.
- **An Arrow array, or its C data interface.** A batch's rows are not
  the window's slots in order, since the source places them by their
  rank in the selection, and a producer other than Parquet, such as the
  heap, has no Arrow array.

## A pointer and a length a row

`data` and `lengths` give each row's bytes wherever they lie, 12 bytes
a row and no copy of a byte; the Parquet source fills them from the
window's offsets for the batch's rows. Offsets into one buffer would
fit a plain Arrow array alone, not a dictionary's values reached
through indexes, and not values in different pages.

## The dictionary's identity

Where the producer holds a column through a dictionary, it gives each
row's index too, and an **identity** of the dictionary: a 64-bit number
from one counter of the backend, which the bridge keeps and gives
through an optional operation of Tessera's API
(`tess_dictionary_identity`). Every producer, each a module of its own,
takes its numbers from that one counter, so no two dictionaries share
an identity. A consumer keeps what it learned of each index, such as a
key's number in TessAgg's dictionary, while the identity stays the
same, and starts again when it changes.

The reader numbers its arrays of values itself: it keeps the last array
it numbered, known by the addresses of its buffers while it holds it,
and takes a new number whenever a window's dictionary is another array.
A chunk read through its dictionary page gives one array to all its
windows. A chunk that falls back to plain pages gives an array of its
own to each array of plain pages, and a window over two of them a
merged one. A dictionary given again after another, as the forward
reader's after a read behind it, gets a new number too. The node turns
each new number of the reader into an identity of the bridge, so that
no call into the reader asks PostgreSQL for anything. An identity stays
in its process: a parallel plan's gather gives no form.

Refused: one number for a chunk's reader, wrong where a reader gives
several dictionaries; the dictionary's address alone, which a later
array may reuse once the first is freed; a counter of each module, two
of which can give one number.

## The check, once a dictionary or a window

The source checks the bytes it gives by the column's rule, so that the
rule "no text unchecked by its column's rule" of the Parquet wrapper
keeps its one owner, the column's read plan of `datum-arrow`. For a
column of text, the plan checks for UTF-8 and zero bytes and counts no
characters, so the check can be made for many rows at once:

- Through a dictionary, the first window that names the dictionary's
  array of values reads the whole array once through the plan; where
  every value passes and none is `NULL`, the windows over it take their
  rows' bytes with no check again.
- A plain window is an array of views, whose values lie in the
  decompressed pages and in the views themselves. Its values are
  checked once a window, however many batches ask it, each value in
  one pass by words of 8 bytes: a zero byte by an exact mask, and only
  a value with a byte at or above 0x80 by the full check of UTF-8.
- Every other case reads the rows asked one by one through the plan, as
  a `Datum` is made: a dictionary with a value that fails, a window that
  fails its pass, a value past PostgreSQL's size, an offset or an index
  that does not hold, `varchar(n)`, trusted text and `bytea`. So a row
  that fails fails at its row with its `Datum`'s error, and a row not
  asked never fails a query.

Refused: bytes given unchecked and checked by each consumer once a new
value, which puts the rule in every consumer, the kernels among them;
and a dictionary's values checked one by one as rows name them, whose
bitmap and scratch arrays cost every batch a pass over the dictionary.

## The window's arrays kept for the batch

A batch's bytes point into the window's Arrow buffers, and a later read
of the column may replace the window while the batch lives, as a read
behind the forward reader does. The node keeps, for each batch, a
reference to every array it gave bytes of, in the reader's crate,
dropped when the batch's memory context is reset, after an error too.
An entry point of the reader fills the pointers and lengths of the rows
asked from the window and takes that reference: the pointers into the
reader's arrays can be taken only in the reader's crate, which owns
them.

## The form passed on

A projection over a batch passes the form on for a column it passes
unchanged from its child, where the child offers it, and gives `false`
for a column it computes; a filter that removes rows of the mask keeps
the producer's form. The other batches Tessera makes give no form:
TessAppend's, over the partitions of a table or the branches of a
`UNION ALL`, and those that gather rows from other batches.

## TessAgg's keys by bytes

The key dictionary gets a lookup by bytes beside its lookup by `Datum`:
the same hash over the same bytes, the same equality by length and
`memcmp`, so that rows of either form find the same number. A value
enters the dictionary as a copy made from its bytes, a header and the
bytes, as a `Datum`'s copy is made.

Through a source's dictionary, each key keeps an array from index to
its number and its hash for the current identity. The array is no
larger than the dictionary's count of values or the key's share of the
grouping's memory, whichever is less, its sizes computed with checks
of overflow, and an index is checked against it before it is read. A
row's index found there costs no hash, and gives the table of groups,
once it is frozen past its memory, the same hash of the value that
divides its spill into parts; an index seen first is looked up by its
bytes once; past its share, a key is looked up by its bytes. The array
is dropped when the identity changes and whenever the key dictionary is
reset: when a spill is read back, and when a partial stage under a
gather sends its table up and starts the next. The ungrouped
`COUNT(DISTINCT)` numbers its values through the same lookup.

A row the frozen table writes to a spill file needs its values: the
node makes them from the bytes for the rows it writes alone, a header
and the bytes, which passed the column's rule when the source gave
them. Asking the source instead checked and copied each string a second
time, nearly a quarter of the instructions of a grouping of strings
that spills. The values of every key of a batch live in one memory
context of the node until the batch's rows are written, and the next
batch resets it.

The node's read of its columns in their order, made before it computes
anything so that a heap batch deforms each row once, leaves out every
column it reads as bytes: a key, the argument of a `COUNT(DISTINCT)`,
a function's argument, which it would otherwise make for every row.
The bytes are read only under a deterministic collation, the condition
of the hash by bytes.

## The kernels over either form

The Rust trait of string sources gets a second source, a
`TessBytesColumn`; the logic of each function stays as it is. A string
argument of the C entry points of `tessera/text.h` is a column of either
form, a field appended to `TessTextArg`. Their only callers,
`kernels/text.c` and the Rust kernels, are built into one library, so
the change of the structure moves no version of an ABI.

A batch function that reads the form says so with a flag,
`TESS_FUNCTION_BYTES`, and its call then carries a pointer for each
argument in a field appended to `TessFunctionCall`, a structure the
consumer passes one of and a function finds by its size. The expression
compiler gives the form for a column it reads unchanged from its input,
to a function with the flag, where the input offers it; a function
without the flag gets `Datum`s. The spec draws both structures.

Some rows of a kernel go to the core's function, with both arguments
as `Datum`s: a `LIKE` pattern the kernels' matcher refuses, for `_`, an
escape or more pieces than it holds, and the characters of a multibyte
encoding other than UTF-8. A row of the bytes form is never compressed;
for every row the kernel sends to the core, it makes a value from the
row's bytes in the call's memory context. A row whose other argument is
compressed or stored apart is compared again by the kernels over that
one row, the compressed value unpacked and the row of the form made a
value the same way.

Refused: a field appended to `TessFunctionArg`, whose array a function
indexes by the size its own header gives, so that a module built before
the field would read the second argument in the wrong place.

## Order by bytes

A helper of the nodes (`nodes/key_kind.h`) asks PostgreSQL's locale of
a collation whether it compares by bytes, true for `C`, `POSIX` and the
builtin provider's collations. TessSort gets a key kind of its own for
such a string key ordered by its type's default B-tree operator class,
and for `bytea` by its default class; another class, such as a
module's, may order otherwise and keeps the generic kind.

The kind's word is the first eight bytes, big-endian, zero-padded, its
top bit flipped, since the sort's words compare as signed integers: so
words compare as the strings' starts do, unsigned. Equal words go to
the type's comparison, which under such a collation compares the bytes
and then the length, and a top-N sort decides the rows whose words
equal the worst's by their full bytes before it asks their values. An
ordered gather, which merges its workers' sorted rows and compares only
the generic kind beyond the words, keeps the generic kind for a string
key.

The word comes from the bytes form where the input offers it, and
otherwise from the `Datum`, unpacked when compressed. A full sort keeps
every row's value in its records, so it reads the `Datum`s and makes
the words from them. In a top-N sort the key's values are asked, with
the other columns, for the rows that beat the worst kept, so that the
source makes values for those rows alone.

Refused: the core's abbreviated key under `C`, which hashes every row
to decide whether to stop abbreviating, a decision a top-N sort never
uses.

## How the code is checked

The form hands a consumer in C pointers into a producer's memory, the
reader's in Rust among them, and gives a second way to results the
`Datum` path gives. A fault can read freed memory, leak a reference, or
put rows into the wrong groups without a sign, so each kind of risk
gets the check that can see it, and the contract names them.

- **Kani, for the logic in Rust.** Bounded proofs: the sort word orders
  two strings, with the comparison of the rest and of the lengths, as
  their unsigned bytes do; the one pass over a window passes exactly
  where each value's check of encoding and zero bytes passes; every
  pointer and length the reader gives from offsets and indexes it
  checked lies inside its buffer. What a harness's bounds leave out is
  named in it and covered by a property test.
- **Miri, for the Rust that holds or reads the pointers.** The reader's
  tests read every pointer it gave after the window is replaced, after
  its own pending arrays are dropped and after other reads, while their
  reference lives.
- **The sanitizers, for the code in C.** ASan takes over `malloc` for
  the whole process, so a read in C of a buffer the reader freed is
  caught, though not a read inside Rust. So each case of a lifetime
  reads the bytes in C: a test module's call keeps the pointers of a
  batch, has the reader replace the arrays they point into, and reads
  every byte before the batch is released; an assert build's
  expression compiler reads the first and the last byte of each row it
  hands a kernel. The Parquet node reads a batch's window once, so an
  injection point makes each ask read the window again, behind the
  reader, which then decodes it anew.
- **An error between taking and dropping a reference.** An injection
  point raises an error after a batch's references are taken, and the
  reader's count of the references alive in the backend must be where
  it was after the reset, as after a rescan and the scan's end.
- **Self-checks of an assert build, against wrong groups and orders.**
  Each hit of TessAgg's array from index to number compares the row's
  bytes with its group's; after a sort and after a merge, each pair of
  adjacent rows is checked by their full bytes, and each comparison of
  ties and each row a top-N sort drops against the worst it keeps; the
  rows the source gives in the bytes form are checked again by the plan
  row by row.
- **One library of collations, the server's.** A node of Tessera sits
  in a plan of the core, under a merge join, a sorted grouping or a
  gather that merges, which compare by the core's functions, so a
  string ordered under a collation must be ordered by them too. No
  module of Tessera links ICU or another library of collations, so a
  backend holds the one copy the server loaded; each place that orders,
  hashes or compares strings by their bytes has its gate listed at
  review.
- **Random queries.** `tessera-crosscheck` draws string columns under
  `C`, the builtin `C.UTF-8`, a libc collation and a nondeterministic
  one, and gives each table of strings a Parquet twin, its rows written
  in dictionary, plain and mixed chunks; each query over the twin is
  compared with the same query over the heap table.
- **The layouts.** The installed headers' check asserts the offsets the
  spec draws, the Rust mirror of `TessBytesColumn` tests its own, and a
  test reads a structure allocated at the size before its last field.
- **A test source of the form.** A source of the C test modules gives a
  table of strings in both forms, with and without dictionaries, and
  with operations of the size before the form, and counts the asks of
  each form, so that TessAgg, the kernels and TessSort are tested on
  the form without the Parquet module. With its own check on, it reads
  each batch it makes as a consumer would.

What no check proves is the C logic of TessAgg and TessSort over the
form: the self-checks, the random queries and the sanitizers stand for
a proof there.

## What it gained

Counted by instructions over ClickBench's file, each part against the
work before it:

- The keys: Q34 and Q35, all rows grouped by `URL`, 21.8 and 21.6 per
  cent fewer; Q13 6.9 per cent fewer, not the 15 hoped, since half of
  the query is the reading of the column, which the form cannot remove,
  and the values the keys no longer make were 7 per cent of it.
- The functions: Q28 28.0 per cent fewer. The heap's own cases of
  `LIKE` take 1.1 per cent more, the checks of an argument's form on a
  call's way, some 50 instructions a batch of 64 rows.
- The sort: Q26 12.4 per cent fewer, not the 15 hoped: 94 per cent of
  the query is the reading of its column, the pages' decompression,
  the check of UTF-8 and the copies, and the values and abbreviated keys
  the sort no longer makes were 12 per cent of it.

## What we decided not to do

- **`min` and `max` of a string.** In ClickBench they appear over the
  rows a `LIKE` passes, whose grouping the core runs, and in a query
  that spends its time in `regexp_replace`.
- **The heap source's form.** Its values point into its pages already:
  there is nothing made a row to save.
- **Joins by a string.** No query of ClickBench joins.
- **The form through TessAppend.** A partitioned Parquet table, or a
  `UNION ALL` of Parquet tables, is read through TessAppend's batches,
  which give `Datum`s.
- **A setting to switch the form off.** The results of either form are
  the same by the spec, and a build without the form measures without
  it.

## Files

- `include/tessera/batch.h`: the structure of the form, the batch's
  operation that fills it, and the consumer's ask of it
- `include/tessera/bridge.h`, `bridge/bridge.c`: the identities of the
  dictionaries
- `include/tessera/function.h`, `include/tessera/text.h`,
  `include/tessera/expr.h`: the flag of a function that reads the
  form, the call's field of its arguments, and a text argument of
  either form
- `runtime/project.c`: the form passed on through a projection
- `runtime/expr.c`: the form given to the functions with the flag
- `parquet/node.c`: the Parquet source's form, its identities and the
  references kept for a batch
- `crates/tessera-parquet/src/bytes.rs`,
  `crates/tessera-parquet/src/c.rs`: the reader's bytes, its check once
  a dictionary or a window, and its entry points
- `nodes/agg_keydict.c`, `nodes/agg_group.c`, `nodes/agg_spill.c`: the
  keys by bytes, the arrays from index to number, the values made for
  the rows a spill writes
- `nodes/key_kind.h`, `nodes/sort.c`, `nodes/sort_topn.c`: the order
  by bytes and its key kind
- `kernels/text.c`, `crates/tessera-kernels/src/text.rs`,
  `crates/tessera-capi/src/c/text.rs`: the string kernels over either
  form, and the sort word of a string
- `test/tessera_scan_source_test.c`: the test source of the form

## Tests

- `test/sql/source_scan.sql`: the test source's form, plain, through
  dictionaries and through one that holds a value at two indexes; the
  identities kept and changed; batches without the operation and a
  column without the form; the form through a projection; the
  consumers' asks of either form counted; groupings, the functions and
  the sorts against the same queries over heap tables.
- `test/sql/parquet.sql`: the Parquet source's form, its pointers kept
  across later asks, the masks of the check of UTF-8 and of
  `varchar(n)`; groupings by text, `varchar` and `bytea` over plain and
  dictionary files; the functions; the order by bytes under `C` and
  other collations; a top-N sort; groups that spill; a partial stage's
  tables sent up in a parallel plan.
- `test/sql/parquet_cancel.sql`: bytes read past a later read of the
  window, through an injection point, and a dictionary given again.
- `test/sql/bridge.sql`: the identities of one backend.
- `crates/tessera-parquet/src/c/tests.rs`,
  `crates/tessera-parquet/src/bytes/tests.rs` and the proofs beside
  them: the reader's bytes, their lifetime and their check.
- `crates/tessera-capi/tests/text.rs` and
  `crates/tessera-kernels/src/text/proofs.rs`: the string kernels over
  both forms, and the sort word against the comparison of bytes.
- `tools/tessera-crosscheck`: random queries over Parquet twins of the
  tables of strings.
