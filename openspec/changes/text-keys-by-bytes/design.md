## Context

See proposal.md, "Why", for the numbers. What the code does today:

- A batch gives a column through one operation, `get_datum_column`,
  into a `TessDatumColumn`: one `Datum` and one `NULL` flag a row. Its
  size is checked against a Rust mirror of it, and arrays of it are
  passed to the kernels with the whole structure as their stride. The
  one second form so far is a numeric column's decimals, fields at the
  end of that structure.
- The Parquet source keeps a window of 1 024 rows of a column as an
  Arrow array, plain `LargeBinary` or a dictionary of it, and makes a
  `text` value for each row a consumer asks: it checks the bytes, then
  copies them behind a header into an arena of the batch.
- TessAgg numbers a string key in a dictionary of its own: it hashes
  the value's bytes with PostgreSQL's `hash_bytes`, compares bytes, and
  copies a value once, when it first appears; its table then holds the
  64-bit number. So a group's value is made once already; the cost per
  row is the source's value and the hash.
- The string kernels' logic in Rust reads its rows through a trait of
  string sources; the one source is a column of `Datum`s, read in place
  behind each header.
- TessSort sorts a string key as a generic key: its word is the core's
  abbreviated key, `varstr_abbrev_convert`, which under `C` copies eight
  bytes and hashes up to 64 bytes a row into an estimate of distinct
  values. A top-N sort reads its key columns for every row and the
  other columns only for the rows that enter its heap.
- No code of Tessera asks whether a collation orders by bytes; the
  checks ask whether it is deterministic.

## Goals / Non-Goals

Goals:

- A string's bytes reach the consumers that need only bytes without a
  value made for each row, through one form of the batch contract.
- Every result stays the one the `Datum` path gives, the errors of the
  check of UTF-8 among them; a batch without the form is read as today.
- The rule of the check keeps one owner: the producer of the form.

Non-goals: see proposal.md, "Not in this change"; and no Arrow array
in the C API, no change of `TessDatumColumn`, no setting.

## The whole

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
            a value (output, spill, heap)
```

A consumer that can read bytes asks for them first; a `false` answer,
or a batch without the operation, sends it to `get_datum_column`, as
today. A consumer that needs values for some rows, a row written to a
spill file, a row that beats the worst of a top-N heap, asks
`get_datum_column` for those rows alone: the source makes values only
for the rows asked. TessAgg makes a group's value from its bytes when
the value enters its dictionary, without asking the source.

## Decisions

### An operation of its own, not fields of `TessDatumColumn`

The form is a structure of its own, `TessBytesColumn`, filled by an
optional operation appended after `release` in `TessBatchOps`, which a
consumer finds by the size of the operations. The spec draws the
structure and says what each field holds.

Refused:

- **Fields at the end of `TessDatumColumn`, as the decimals are.**
  Its size is the stride of every array of columns the kernels read and
  is checked against the Rust mirror: 32 bytes more on every column of
  every array, for a form few columns have.
- **An Arrow array, or its C data interface.** The batch's rows are not
  the window's slots in order (the source places them by their rank in
  the selection), and a producer other than Parquet, such as the heap,
  has no Arrow array. Arrow batches in the nodes are the roadmap's
  `direct-arrow-batches`, a wider change.

### A pointer and a length a row

`data` and `lengths` give each row's bytes wherever they are; the
Parquet source fills them from the window's offsets for the batch's
rows, 12 bytes a row and no copy of a byte. Offsets into one buffer
would fit a plain Arrow array alone, not a dictionary's values reached
through indexes, and not values in different pages.

### The dictionary's identity: a number of the backend

A dictionary is known by a 64-bit number from one counter of the
backend, which the bridge keeps and gives through an optional operation
of `TessApi`, so that two producers, each a module of its own, never
give one number. The reader numbers its values arrays itself: it keeps
the last array it numbered, known by the addresses of its buffers while
it holds it, and takes a new number of its own whenever a window's
dictionary is another array. A chunk read through its dictionary page
gives one array to all its windows; a chunk whose footer does not say
how its pages are encoded, and which falls back to plain pages, gives
an array of its own to each array of plain pages, and a window over two
of them a merged one; a dictionary given again after another, as the
forward reader's after a read behind it that decoded the dictionary
anew, gets a new number too. The node turns each new number of a
reader into an identity of the bridge, so that no call into the reader
asks PostgreSQL for anything. A consumer keeps what it learned of each
index, a key's number in TessAgg's dictionary, while the identity stays
the same, and starts again when it changes. An identity stays in its
process: a parallel plan's gather gives no form. Refused: one number
for a chunk's reader, wrong where a reader gives several dictionaries;
the dictionary's address alone, which a later array may reuse once the
first is freed; a counter of each module, two of which can give one
number.

### The check: the producer's, once a dictionary or a window

The source checks the bytes it gives by the column's rule, so that the
rule of `parquet-fdw`'s contract, no text unchecked by its column's
rule, keeps its one owner: the column's read plan of `datum-arrow`. For
a column of text the plan checks for UTF-8 and zero bytes and counts no
characters of, the check costs less. For a chunk read through its
dictionary, the first window that names the dictionary's values array
reads the whole array once through the plan; where every value passes
and none is `NULL`, the windows over it take their rows' bytes with no
check again. For a plain window, one pass checks the encoding and the
zero bytes of the window's bytes, all its rows' together, and each
value's start for a byte that continues a character; the pass is made
once a window, however many batches ask it. Every other case, a
dictionary with a value that fails, a window that fails its pass, a
value past PostgreSQL's size, an offset or an index that does not hold,
`varchar(n)`, trusted text and `bytea`, reads the rows asked one by one
through the plan, as a `Datum` is made, so that a row that fails fails
at its row with its `Datum`'s error and a row not asked never fails a
query. Refused: bytes given unchecked and checked by each consumer once
a new value, which puts the rule in every consumer, the kernels' among
them; and a dictionary's values checked one by one as rows name them,
whose bitmap and scratch arrays cost every batch a pass over the
dictionary.

### The window's arrays kept for the batch

A batch's bytes point into the window's Arrow buffers, and a later read
of the column may replace the window while the batch lives, as a read
behind the forward reader does. The node keeps, for each batch, a
reference to every array it gave bytes of, in the reader's crate,
dropped when the batch's memory context is reset, after an error too.
The reader's crate gets an entry point that fills the pointers and
lengths of the rows asked from the window and takes that reference.
This departs from the forecast of `parquet-fdw`'s contract, which named
the node's batches and not the reader: the pointers into the reader's
arrays can only be taken in the reader's crate. The pull request says
so under "Architecture", updates the forecast, and opens the roadmap's
entry for simplifying the reader's boundary with the node, naming that
boundary, as the rules of contracts ask of a first departure.

### TessAgg's keys by bytes

The key dictionary gets a lookup by bytes beside its lookup by
`Datum`: the same `hash_bytes` over the same bytes, the same equality
by length and `memcmp`, so that rows of either form find the same
number. A value enters the dictionary as a copy made from its bytes, a
header and the bytes, as a `Datum`'s copy is made today. Through a
source's dictionary, each key keeps an array from index to its number
and its hash for the current identity, no larger than the dictionary's
count of values or the key's share of the grouping's memory, whichever
is less, its sizes computed with checks of overflow, and an index
checked against it before it is read; past its share, a key is looked
up by its bytes: a row's index found there costs
no hash, and gives the frozen table the same hash of the value that
divides its spill into parts; an index seen first is looked up by its
bytes once. The array is dropped when the identity changes and
whenever the key dictionary is reset: when a spill is replayed, and
when a partial stage under a gather sends its table up and starts the
next. The ungrouped `COUNT(DISTINCT)` numbers its values through the
same lookup. A row the frozen table writes to a spill file needs its
values: the node asks `get_datum_column` for the rows it writes alone.
The node's read of its columns in their order, made before it computes
anything so that a heap batch deforms each row once, leaves out every
column it reads as bytes, a key, the argument of a `COUNT(DISTINCT)`
or a function's argument, which would otherwise be made for every
row.
The gate stays as it is: bytes only under a deterministic collation,
the condition of the hash by bytes today.

### The kernels over either form

The Rust trait of string sources gets a second source, a
`TessBytesColumn`; the logic of each function stays as it is. A string
argument of the C entry points of `tessera/text.h` becomes a column of
either form, a field appended to `TessTextArg`; the entry points that
take a bare column take a `TessTextArg`. Their only callers,
`kernels/text.c` and the Rust kernels, are built into one library, so
the change of the structure moves no version of an ABI. A batch
function that reads the form says so with a flag,
`TESS_FUNCTION_BYTES`, and its call then carries a pointer for each
argument in a field appended to `TessFunctionCall`, a structure the
consumer passes one of and a function finds by its size:
`TESS_FUNCTION_ABI_VERSION` stays. Refused: a field appended to
`TessFunctionArg`, whose array a function indexes by the size its own
header gives, so that a module built before the field would read the
second argument in the wrong place. The expression compiler gives the
form for a column it reads unchanged from its input, to a function
with the flag, where the input offers it. A function without the flag
gets `Datum`s, as today. The spec draws both structures.

Some rows of a kernel go to the core's function today, with both
arguments as `Datum`s: a `LIKE` pattern the kernels' matcher refuses,
for `_`, an escape or more pieces than it holds, and a row whose other
argument is compressed or stored apart. A row of the bytes form is
never compressed itself; for every row the kernel sends to the core,
it makes a value from the row's bytes in the call's memory context and
calls the core's function, as it calls it today with the row's
`Datum`.

### Order by bytes

A helper of the nodes asks PostgreSQL's locale of a collation whether it
compares by bytes (`collate_is_c`), which is true for `C`, `POSIX` and
the builtin provider's collations. TessSort gets a key kind of its own
for such a string key ordered by its type's default operator class of
B-tree, and for `bytea` by its default class: another class, such as a
module's, may order otherwise, and keeps the generic kind. Its word is
the first eight bytes, big-endian, zero-padded, its top bit flipped,
since the sort's words compare as signed integers, as the generic kind
flips the core's abbreviated key: so words compare as the strings'
starts do, unsigned. Equal words compare by `memcmp` of the rest and
then by length. An ordered gather, which merges its workers' sorted rows
by the same keys and knows how to compare only the generic kind beyond
the words, keeps the generic kind for a string key. The word comes from
the bytes form where the input offers it, and otherwise from the
`Datum`, unpacked when compressed. In a top-N sort the key's values are
asked, with the other columns, for the rows that beat the worst kept, so
that the source makes values for those rows alone. Refused: keeping the
core's abbreviated key under `C`, which hashes every row to decide
whether to stop abbreviating, a decision a top-N sort never uses.

### How the code is checked

The form hands a consumer in C pointers into a producer's memory, the
reader's in Rust among them, and gives a second way to results the
`Datum` path gives; an error can read freed memory, leak a reference,
or put rows into the wrong groups without a sign. Each kind of risk gets
the check that can see it; the contract of the capability names them.

- **Kani, for the logic in Rust.** Bounded proofs, run by CI's job of
  Kani within its 600 seconds a harness: the sort word's kernel orders
  two strings, with the comparison of the rest and of the lengths, as
  their unsigned bytes do; the one pass over a window passes exactly
  where each value's check of encoding and zero bytes passes; every
  pointer and length the reader gives from offsets and indexes it
  checked lies inside its buffer. Each harness takes the bounds that
  finish, as the harnesses of `datum-arrow` and of the kernels' sort
  do, comparing slices by hand where Kani cannot follow the library's
  comparison; what its bounds leave out is named in it and covered by a
  property test. Kani sees neither the code in C nor a lifetime across
  the boundary.
- **Miri, for the Rust that holds or reads the pointers.** The reader's
  tests read every pointer it gave after the window is replaced, after
  its own pending arrays are dropped and after other reads, while their
  reference lives, so that Miri fails them if the reader freed or moved
  a buffer; the string kernels' source over the form is tested under
  Miri too. CI's job of Miri runs both.
- **The sanitizers, for the code in C.** CI's job of ASan and UBSan
  builds every module, the Parquet one among them, and runs the suites;
  ASan takes over `malloc` for the whole process, so a read in C of a
  buffer the reader freed is caught, though not a read inside Rust,
  which is not instrumented. So each case of a lifetime reads the bytes
  in C: a test module's call keeps the pointers of a batch, has the
  reader replace the arrays they point into, and reads every byte
  before the batch is released; and an assert build's expression
  compiler reads the first and the last byte of each row it hands a
  kernel. The Parquet node reads a batch's window once, and nothing
  else a reader holds changes while the batch lives, so no query would
  show a reference dropped early. An injection point of the node makes
  each ask read the batch's window again, behind the reader, which then
  decodes it anew, and a column through a dictionary names a second one.
  The forward reader keeps what it read forward, so the test asks the
  column as `Datum` values first: its ask in the bytes form is then a
  read behind too, and once the later asks replace that read, its bytes
  live by their reference alone. The commit shows by a mutant that drops
  the batch's reference early that the suite under ASan fails.
- **An error between taking and dropping a reference.** ASan's job does
  not look for leaks, so an injection point raises an error after a
  batch's references are taken, and the reader's count of the
  references alive in the backend must be where it was after the reset,
  as after a rescan and the scan's end; a mutant that never drops them
  fails the counts.
- **Self-checks of an assert build, against wrong groups and orders.**
  PostgreSQL is built with assertions for CI's suites. Each hit of
  TessAgg's array from index to number compares the row's bytes with
  its group's; after a sort and after a merge, each pair of adjacent
  rows is checked by their full bytes, and each comparison of ties and
  each row a top-N sort drops against the worst it keeps; the rows the
  source gives in the bytes form are checked again by the plan row by
  row, by the node through the entry that sizes text, and, in a debug
  build of the reader, as CI's cells of debug kernels build it, inside
  the reader too, where a difference is an error of its own. The words
  themselves are compared in a kernel, where C does not see them;
  equal words of unequal strings are ties, which the full comparison
  decides.
- **One library of collations, the server's.** A node of Tessera sits
  in a plan of the core, under a merge join, a sorted grouping or a
  gather that merges, which compare by the core's functions, so a string
  ordered under a collation must be ordered by them too. CI's job of the
  suites runs `.github/scripts/check-collation.sh` after the build: no
  module of Tessera links ICU or another library of collations, and no
  crate of its Rust code is one, so a backend holds the one copy the
  server loaded; the script also fails on the server's binary, which
  links ICU, so that it is seen to look. Where a node orders, hashes or
  compares strings by their bytes, each place's gate is listed at
  review: `collate_is_c` for an order, a deterministic collation for
  equality.
- **Random queries.** `tessera-crosscheck`, which CI runs, gets a
  cluster of `UTF8`, string columns and the collations `C`, the
  builtin `C.UTF-8`, a libc one and a nondeterministic one; the rows of
  a query with `ORDER BY` are compared in order, made total by every
  output column, and the groups of a nondeterministic collation by
  their counts. Each table of strings gets a Parquet twin, its rows
  written by the reader's crate in dictionary, plain and mixed chunks,
  and each query over the twin is compared with the same query over the
  heap table, so that a fault of the reader, which both modes over the
  twin share, is seen too. Errors are left to the suites, which compare
  their SQLSTATEs.
- **The layouts.** The installed headers' check asserts the offsets the
  spec draws, the Rust mirror of `TessBytesColumn` tests its own, and a
  test reads a structure allocated at the size before its last field.
- **Property tests and mutants**, as every Rust change has them: each
  kernel over both forms of the same strings, the reader's check
  against `datum-arrow`'s, the sort word against the comparison of
  bytes; the mutants of CI over the changed lines.

What no check proves: the C logic of TessAgg and TessSort over the
form. The self-checks, the random queries and the sanitizers stand for
a proof there.

### A test source of the form

`test/` gets a source of the C test modules that gives a table of
strings in both forms, with and without dictionaries, and counts the
asks of each form, so that TessAgg, the kernels and TessSort are tested
on the form without the Parquet module, and the Parquet suite tests the
source's own form.

### Decisions of the maintainer

- 2026-11-21: grouping by a string on the file's bytes is a change of
  its own after the Parquet wrapper's parallel reading and its queries
  that read no string (`text-keys-by-bytes` in the roadmap).
- 2026-11-23: the string functions and the order by bytes join the
  keys in this one change, rather than a change of their own.
- 2026-11-23: the checks of the code as "How the code is checked" says
  them, and the proposal, approved; the work goes into the proposal's
  pull request, #172, rather than four of its own.
- 2026-11-22 and 2026-11-23: the stages of speed are counted first,
  cycles and instructions by `pg_pmu`, the verdict by instructions,
  which another session's load does not change; times come when the
  maintainer says the work by counts is done.

## Was and will be

- **The batch contract.** Was: `Datum` values alone, and a numeric's
  decimals. Will be: also the bytes of a string column, optional.
- **The Parquet source.** Was: a `text` value made for every row asked.
  Will be: the bytes form for every string column; values made only for
  the rows a consumer asks as `Datum`s.
- **TessAgg.** Was: a string key's value made by the source each row,
  hashed each row. Will be: its bytes hashed each row, or each index
  once a dictionary; its value made once a group.
- **The string kernels.** Was: `Datum` columns. Will be: either form.
- **TessSort.** Was: the core's abbreviated key for a string. Will be:
  under a collation that orders by bytes, a word of the first bytes and
  ties by bytes; values for the kept rows alone in a top-N sort.

## What we do not do

- **`min` and `max` of a string.** In ClickBench they appear in Q22
  and Q23, over the rows a `LIKE` passes, whose grouping the core runs
  today, and in Q29, which spends its time in `regexp_replace`.
- **The grouped `COUNT(DISTINCT)`.** `agg-distinct-spill` takes it;
  when TessAgg runs one, its key reads bytes by this change.
- **The heap source's form.** Its values point into its pages already:
  there is nothing made a row to save.
- **Joins by a string.** No query of ClickBench joins.
- **The form through TessAppend.** A partitioned Parquet table, or a
  `UNION ALL` of Parquet tables, is read through TessAppend's batches,
  which give `Datum`s: no query of ClickBench reads one.
- **A setting to switch the form off.** A base build measures without
  it, and the results of either form are the same by the spec.

## Risks / Trade-offs

- [A pointer kept past its batch reads freed memory] → the spec's rule
  of lifetime, the reference kept for the batch, a test that asks the
  same column twice across a read behind the forward reader, and the
  suites under the sanitizers of CI.
- [Two forms give different results] → every test compares with the
  core, over the test source and the Parquet table, through both forms.
- [A row not asked fails the check] → the window's pass falls back to
  the rows asked; a test with a bad value outside the mask.
- [The index array of a key grows with a large dictionary] → it is
  bounded by the dictionary's values in the chunk and counted in the
  grouping's memory; past `hash_mem`'s share it is dropped and keys are
  looked up by bytes.
- [The sort's word differs from the core's order under some collation]
  → the kind is taken only where PostgreSQL itself compares by bytes;
  tests under `C`, `C.UTF-8` and a libc collation.

## Measurement plan

On the M5 Pro, with `cargo clickbench run --counters --tries 5 --mode
on`, one query a run, `--workers 0`, over `hits.parquet`, PostgreSQL 19
with `pg_pmu`: instructions decide, cycles are reported beside them.
Each stage of the work, the form, the keys, the functions and the sort,
counts every query of ClickBench that reads a string, and the 17 that
read none as controls, against two bases: the commit the stage starts
from, which judges what it alone changed, and stage 4 of
`parquet-fdw`, which shows the whole change so far. The
heap's cases of strings are counted against the first by `cargo ab
measure --counters --base <the commit> --family sort --family anykey`.
The queries of each stage:

- the form: none, since no consumer reads it yet;
- keys: Q6, Q13, Q15, Q17, Q18, Q19, Q34, Q35, Q37, Q38, Q39, Q40;
- functions: Q28;
- sort: Q26, Q27, and the heap's `order_text_c` and `topn_text_c`.

Hypotheses, written before the runs: Q13 20 to 35 per cent fewer
instructions; Q26 25 to 40; Q28 20 to 30; Q34 and Q35 15 to 30; the
others of each list fewer. Against the commit it starts from, a stage
is kept when its first query of the list above, Q13, Q28 or Q26, loses
at least 15 per cent, its other queries lose some, and every other
query counted, of ClickBench and of the heap, gains no more than 1 per
cent; the first stage, of the form, only by the last rule; otherwise
its run's
report says which way failed and why, and the maintainer decides. Each
run's report is kept under `docs/benchmarks/`, as `docs/measuring.md`
says.

## Open Questions

None that changes the specs, the approach or the tasks.
