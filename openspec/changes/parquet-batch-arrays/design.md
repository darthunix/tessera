# Design

## Context

The Parquet node, `TessParquetScan`, publishes a group's selected rows
in batches of up to 64 rows. A consumer asks a batch for a column, as
`Datum` values and `NULL` flags, or a string column as its bytes. The
node makes the values of the rows asked that it has not made yet, keeps
them, and marks them in a word of the rows made, so that a later ask of
the same batch makes only the new rows.

Each batch's arrays live in a memory context of the node, which
`next_batch` resets before it makes the next batch. Then it allocates,
zeroed, three arrays with an entry for each attribute of the foreign
table:

- `values`, a pointer to each column's array of values;
- `isnull`, a pointer to each column's array of flags;
- `made`, each column's word of the rows made.

`hits` has 105 attributes, so these are 2 520 bytes a batch, whatever
the query reads. `batch_column` then allocates, zeroed, a column's
array of `Datum` values and its array of flags, of the batch's count of
rows, at the batch's first ask of it: 576 bytes for a full batch.
`batch_bytes` allocates, at the batch's first ask in the bytes form, an
array of a view for each attribute, 840 bytes, and for each column
asked so the view and four arrays of the batch's rows, 1 088 bytes for
a full batch. Sizes here are the arrays' own bytes, without the
allocator's.

The memory reset at each batch also holds what does live for one batch
only: the arena of a string column's values made as `Datum` values,
the references that keep a window's arrays, whose bytes the bytes form
points into, and, under assertions, the scratch of the check that
`make_bytes` runs and frees.

The batch's API (`include/tessera/batch.h`) says that the rows a
consumer did not ask hold values of no meaning, which a consumer may
load and must not interpret, and that every row below the batch's count
is written memory. For the bytes form it promises a row's place and
length only for a row asked that is not `NULL`. The heap's provider
already leaves stale values in the rows a selection drops. The node
zeroes its arrays only to keep every row written.

A profile of ClickBench's Q1, `SELECT COUNT(*) FROM hits`, puts these
allocations and their zeroing at about half of the query; a profile of
Q3, `SUM`, `COUNT` and `AVG` over two integer columns, at a tenth. The
100 million rows of `hits` are 1.56 million batches.

## Goals / Non-Goals

**Goals:**

- No allocation and no zeroing at a batch for a query that reads no
  column; for a query that reads columns, a few words cleared for each
  column a batch, where there were allocations and zeroing for each
  attribute of the table and each column asked.
- The same rows made, values, flags, bytes and dictionaries for every
  ask, and the same lifetime of what a consumer gets: until the batch
  is released.

**Non-Goals:**

- How a column's values reach the batch: the reader's call for each
  batch of 64 rows and the copy into the batch's arrays, which the
  entry `parquet-integer-columns` holds.
- The arrays of the wrapper's rows, the clauses and the reader, which
  are made a group or a window, not a batch.

## Decisions

A batch's memory, now and after the change:

```
 now: at each batch
   reset of the batch's memory
   values[105], isnull[105], made[105]      zeroed: 2 520 bytes
   at a column's first ask:
     values[rows], isnull[rows]              zeroed: 576 bytes
   at the first ask in the bytes form:
     a view for each of the 105 attributes   zeroed: 840 bytes
     at each column's: a view, 4 arrays      zeroed: 1 088 bytes

 after: at the scan's begin, n the scan's columns
   values[n], isnull[n], made[n], views[n]
   at a column's first ask in the scan:
     its arrays of 64 rows, zeroed once
 at each batch
   made[0..n) = 0; each view: rows made, dictionary = 0
   under assertions: the values and the bytes' places wiped
   reset of the batch's memory: the arenas of strings, the
   references to windows, the checks' scratch, where a batch made them
```

### The arrays kept for the scan

- **Was:** `next_batch` allocates `values`, `isnull` and `made` for
  each attribute of the table in the batch's memory, and the asks
  allocate the column's arrays there.
- **Will be:** `node_begin` allocates them in the query's memory for
  each column the scan reads, by the scan's number of the column, as
  the node's other arrays of columns are kept. Their count is the
  plan's columns, which `node_begin` reads before a parallel worker
  has its scan. A column's arrays of 64 rows, in either form, are
  allocated zeroed at its first ask in the scan, and kept. 64 rows, not
  the batch's count: a column first asked in a short batch, the last
  of a window or a group, serves the full batches after it.
  `next_batch` asserts that a batch holds at most 64 rows.
- **Why the scan's columns:** a batch is asked only for the columns the
  plan reads, which the node already numbers; an entry for each
  attribute serves no ask. The number of the scan's column is what
  `batch_column` and `batch_bytes` already look up.
- **Why at the first ask:** a column of a clause the source checks may
  never be asked, and the form a string column is asked in is known at
  its ask. The first ask's zeroing keeps every row written memory, as
  the batch's API wants.

### What each batch clears

- **Was:** a batch's rows made, and its views' dictionaries, start at
  zero because the arrays are new.
- **Will be:** `next_batch`, and a rescan, clear for each of the scan's
  columns the word of the rows made as values and its view's rows made
  and dictionary; a view's count of values is read only with a
  dictionary. An ask that makes no row, of an empty mask, then answers
  without a dictionary, as now.
- An asked `NULL` row of the bytes form gets a place, a length and an
  index of 0, as the zeroed arrays gave it; an asked `NULL` row of
  values gets 0 already.
- The values, flags and bytes of a row a batch did not make are an
  earlier batch's. A string's `Datum` or place there may point into an
  arena or a window freed since: such a row is of no meaning, and a
  consumer that keeps to the API does not read through it.

### The arrays wiped under assertions

A consumer that kept a column's arrays past the batch's release read
freed memory before the change, which PostgreSQL's builds with
assertions wipe; after it, the next batch's values would look valid.
Under assertions, `next_batch` and a rescan therefore write the byte
`0x7F`, as PostgreSQL's wipe does, into every kept array of values and
of the bytes' places. Such a consumer reads the wipe where the next
batch has not made the row again: an aid to the suites, not a proof.
The flags, lengths and indexes are left: a consumer may load a flag of
a row it did not ask, and a `bool` of another byte is not a value. A
build without assertions does none of it.

### The memory reset at each batch

The reset stays, for the arenas of strings, the references to windows
and the checks' scratch. A batch that allocates nothing leaves the
context empty, and PostgreSQL's reset of an empty context returns at
once.

### Refused

- **The arrays for each attribute, kept for the scan.** It would clear
  a word for each column read by the table's attribute numbers, with a
  table of the scan's columns to walk; the scan's own numbers need
  none, and the arrays hold no entry no ask reads.
- **Arrays sized for the batch, allocated at each batch from a cheaper
  allocator.** Still an allocation and a zeroing a column a batch,
  where the arrays can be kept.
- **No zeroing at all.** The first batch's rows not made would be
  memory never written, which the batch's API forbids: a consumer may
  load them.
- **The batch pointing at the window's values without a copy.** A
  change of how the values reach the batch, which is
  `parquet-integer-columns`'s.

## Measurement plan

On the M5 Pro, `hits.parquet` in the page cache: `cargo clickbench run
--counters --tries 5 --workers 0`, one query a run, PostgreSQL 19 with
`pg_pmu`. The base is the commit the work starts from, the end the
commit of 2.1, each in a worktree of its own, counted in turns: base,
end, base, end.

Before the runs:

- the agent asks the maintainer for an idle machine and records the
  power source;
- each worktree's release build is installed and the library the server
  loads checked;
- the plans of the base and the end are compared, query by query.

Queries, with Tessera on:

- Q1, `COUNT(*)`, which reads no column;
- Q3, `SUM`, `COUNT` and `AVG` over two integer columns;
- Q7, the least and the greatest of one date column;
- Q28, which groups by an integer column and averages the length of a
  string column read in the bytes form: a guard, since its arrays a
  batch are about one per cent of its 26 thousand cycles a batch.

The control: Q1 with Tessera off, `--mode off`, which reads the same
file through the wrapper's rows and no batch of the node.

What is read:

- A run's count is the median of its five readings, as the tool gives
  it, with the spread of the readings beside, the maximum less the
  minimum over the median.
- A change is the end's count less the base's over the base's, in each
  pair of turns. Cycles decide, by the maintainer's rule; instructions
  stand beside them, and pg_duckdb's cycles at one thread of the runs
  of 2026-11-24, on the same server and file.
- A side's spread is the difference of its two turns. A change of
  cycles smaller than the larger spread of the two sides, or than the
  control's move, is no change.

Hypotheses, written before the runs:

- Q1: 30 to 55 per cent fewer cycles, the profile's half; fewer
  instructions.
- Q3: 5 to 12 per cent fewer cycles, the profile's tenth.
- Q7: 7 to 15 per cent fewer cycles. Not profiled: the saving of Q1 a
  batch, about 105 cycles, and of one column of Q3's two, about 85,
  against Q7's 1.6 thousand cycles a batch.
- Q28: within 2 per cent.
- The control: within 2 per cent.

How the change is judged:

- When a side or the control spreads past 2 per cent, that query is
  counted once more, a third pair of turns; the first two are kept, and
  the side's spread is then the range of its three turns. Still past 2,
  the query is unsettled, not passed or failed.
- The change is kept when Q1 loses at least 25 per cent of its cycles,
  Q3 at least 4, and no query gains more than 2. A result past a
  threshold and outside its hypothesis passes, and is explained in the
  report. An unsettled query is not a pass: the maintainer decides.
- A confirmed excess, or a threshold missed, is profiled and searched
  as CONTRIBUTING.md says before the maintainer is asked.
- Profiles of Q1 and Q3 at the base and at the end, by `sample`, are
  kept in the report beside the counts.

The report is kept under `docs/benchmarks/`, as `docs/measuring.md`
says.

## Risks / Trade-offs

- [A consumer that reads a row it did not ask and counts on a zero]
  → it would get an earlier batch's value where it got 0. The API
  forbids it, the heap's provider already gives stale values, and the
  suites and `make crosscheck` compare results with Tessera off.
- [A consumer that keeps a column's arrays past the batch's release]
  → it would read the next batch's values where it read freed memory.
  No consumer does: each copies what it keeps before it finishes a
  batch. Under assertions the arrays are wiped at each batch, so that
  the suites see it as before.
- [The rows made not cleared for a column] → a later batch would give
  an earlier batch's values as its own. The test module's consumers
  ask the batches before the one they check, in both forms, so that
  such a value differs from the heap table's.
- [Arrays first allocated for a short batch] → a full batch after it
  would write past them. They are of 64 rows; a test asks a column
  first in a short batch and then in a full one, in both forms.
- [A view's dictionary of an earlier batch] → cleared at each batch.
  The changes of a dictionary between batches are checked by
  `parquet_cancel`'s grouping over windows read again at each ask, as
  now.
- [Memory kept for the scan] → 576 bytes a column asked, and 1 088 a
  column asked in the bytes form, for the query's life, against as much
  allocated and freed at each full batch.

## Decisions of the maintainer

- Cycles decide the counts; instructions help but come second.

## Files

- `parquet/node.c`: the node's batch.
- `test/tessera_parquet_test.c`: the consumers of a batch that ask the
  batches they skip.
- `test/sql/parquet.sql`, `test/sql/parquet_cancel.sql` and their
  expected output: the new queries.
- `openspec/specs/parquet-fdw/design.md` and
  `openspec/specs/parquet-fdw/contract.md`, at the close.

## Tests

- `parquet_test_asks` and `parquet_test_bytes` take an optional last
  argument, false by default, to ask every target's column of each
  batch they skip before the batch they check: the first as `Datum`
  values, the second in the bytes form, again after its rescan. The
  suite's calls stay as they are, among them those that skip six
  batches of ten rows, where a column's first read of a window is an
  ask in its middle; each such call is made once more with the
  argument true. The rows the checked batch asks were then made by the
  batch before it, at the same places, so that a row not made again
  would give the earlier batch's value and differ from the heap
  table's. `parquet_cancel` makes such a call with each ask reading
  its window again and a rescan.
- After those six batches asked in the bytes form through a dictionary,
  an ask of no row of the seventh names no dictionary.
- `parquet_test_bytes` fails on an asked `NULL` row of the bytes form
  that has a place, a length or an index; the rows asked after the six
  batches change from a value to `NULL` at some places.
- A table over a file of a group of three rows and then the file of
  `t_asks`, against a heap table of the same rows: an integer, a time
  and a `text` column read as values, and the `text` column in the
  bytes form, first asked in the batch of three rows and then in full
  batches up to row 63.
- The suites `parquet` and `parquet_cancel` as they were: one column
  asked again, values kept across the asks of a batch, the bytes form,
  its references and its dictionaries given again, a rescan for each
  outer row, parallel scans; and `make crosscheck`, whose table `words`
  has a Parquet twin.
