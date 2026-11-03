## Context

`deform` in `runtime/heap_batch.c` deforms one value of one row:
`tess_deform_advance` moves the row's `TessDeformCursor` from the
attributes it has passed to the requested one, and returns that value
only. A column before the cursor goes through the row's second cursor,
which starts from the tuple's start when it has passed the column too.
The batch remembers the values it deformed (a mask a column), so only a
column not yet read for a row costs a walk. A request names its rows:
a consumer above a filter asks only for the rows the filter kept, so
the columns of dropped rows are never deformed.

What a walk costs, measured on TPC-H's `lineitem` at SF 1 (6 M rows):
the columns its queries read past attribute 4 lie after four `numeric`
and two `char(1)`, each a value of varying length with a one-byte
header. A walk to `l_shipdate` reads six headers; the machine code
spends about 17 instructions on each, and each offset waits for the
length before it. Four rows walked in step, an attribute of each at a
time, ran no faster: the processor already overlaps the walks of
consecutive rows. What can go is the number of walks and of
instructions.

## Goals / Non-Goals

**Goals:**

- One walk of a row for the columns its consumers read, in whatever
  order they ask for them.
- No store of a value that its consumers rarely read: a filter that
  drops most rows keeps its cost.
- No cost on a scan that reads one column a row.
- The walk to a column past values of varying length cheaper, if a
  measurement shows it.

**Non-Goals:**

- No change of `tess_deform_advance`, of `heap_deform.h` or of the C
  API: the walk that keeps values lives in `runtime/heap_batch.c`.
- No reordering of the clauses or of the columns a consumer asks for.
- No deform in Rust or with vector instructions, decided against
  before.

## Decisions

### A walk that keeps the columns read behind it

A scan keeps a flag a column, `kept`. A walk forward to a column, from
the row's cursor, stores the value of every kept column it passes, and
the requested column as before. Columns before the tuple's cached
offsets are not kept: a by-value one is read at its offset, without a
walk.

Example, Q01: the filter asks for `l_shipdate`, attribute 11, on every
row; the aggregate then asks for attributes 5 to 10 on the 98 % of rows
the filter kept. Today each of those rows walks twice: to attribute 11,
then from the start again for the aggregate. With attributes 5 to 10
kept, the first walk stores them, and the aggregate walks nothing.

The flags do not change within a batch, so a row's cursor tells which
kept values the row holds: every kept column before it. A request for a
kept column behind the cursor takes the stored value and starts no
walk. The walk sets no bit of the batch's mask a stored value: the
request sets it, as it does today. So a value stored and never read
costs one store, and nothing else. For this to hold, every forward step
of a cursor stores the kept columns: rows with NULLs, by their bitmap,
and rows shorter than the column, by the attribute's missing value, as
`tess_deform_advance` reads them. While the scan keeps no column, the
walk is today's.

Refused: deforming every column of the scan's targets for every row at
its first request. A filter that drops most rows on its first clause
would pay for the columns of rows it drops; a kept value is a value the
walk passes anyway.

### Which columns are kept: the share of rows that read them

A column is kept when its consumers read it on most of the rows whose
walk passed it. Two counts per column, over a window of batches:

- **reached**: the rows whose cursor ended the batch past the column,
  that is, the rows a walk carried over it or to it;
- **read**: the rows a consumer asked the column for, the batch's mask
  of the column at its end.

Both are taken when the batch is reset: `read` as the popcount of the
column's mask; `reached` from the cursors, the rows counted by where
their cursor stopped, then summed from the last column back. That is
one pass over the rows and one over the columns a batch, and no work a
value. A scan that has asked for one column past the cached offsets
only takes no counts: it has nothing to keep.

When the window reaches 4096 rows, each column with `reached` above
zero is kept for the next window if `read` is at least half of
`reached`; the counts start again. A column no walk passed keeps its
flag. The first window keeps nothing, so a scan shorter than a window
runs as today. The window and its decision follow the key filter's in
`nodes/filter.c`, with a constant of its own: the runtime does not
include the nodes.

In Q01 the aggregate's columns come out at 0.98 and are kept. In Q14
the filter asks for `l_shipdate` on 6 M rows and the aggregate for
`l_extendedprice` and `l_discount` on the 76 000 it keeps: 0.013, not
kept, and Q14 walks as today. A column read on every row it is reached,
such as the filter's own, comes out at 1; keeping it costs nothing,
since no walk passes it unread.

The half is a guess. A kept value costs a store a row; a column not
kept costs, on each row read behind the cursor, a walk from the start,
about 17 instructions a value of varying length. The break-even share
is lower than half when the walk is long; the measurement on Q01, Q06,
Q12 and Q14 sets the constant.

Refused: keeping every column a consumer ever asked for, as the
prototype did. Q14 paid 2 to 3 % for stores of values read on 1.3 % of
rows.

Refused: a count a row at each walk. The cursors already hold where
each walk stopped.

### The cost of a value of varying length

The walk's step for a value of varying length is `align_fetch_then_add`
of PostgreSQL: it checks for a padding byte, for an external value and
for a four-byte header. A path for a one-byte header, the form of every
short value, is tried in the walk and measured on Q06 and Q14; it stays
only if it gains there, and a value of any other form takes the
general step.

## Risks / Trade-offs

- A scan whose consumers change what they read within a scan, such as
  an inner side rescanned with other parameters, learns again over a
  window, paying restarts or stores until it does.
- A row with a kept column behind its cursor that was not stored would
  give a wrong value. The invariant (every forward step stores the kept
  columns) is checked with assertions in builds with them, and the
  tests cover rows with NULLs and rows shorter than the column.

## Measurement

TPC-H at SF 1, base and change in turn, base, change, base, change,
serial and with two workers: Q01, Q12 and Q21 (several columns a row),
Q03, Q06, Q10, Q14 and Q19 (one or two), with their `Deformed Datums`
and `Restarted Datums`; the family `win` against the base; profiles of
Q01 and Q14 before and after.

## Files

- `runtime/heap_batch.c`: the walk that keeps columns, the counts and
  the window.
- `docs/runtime.md`: how a batch deforms.

## Tests

- `test/tessera_heap_batch_test.c`: a later column first, then earlier
  ones on most rows, with no restart after the first window; the same
  read on few rows, with restarts and no kept column; rows with NULLs;
  rows shorter than the column; a reset batch.
- `test/sql/heap_batch.sql`: a filter and an aggregate over a table of
  `lineitem`'s layout, `Restarted Datums` near zero.
