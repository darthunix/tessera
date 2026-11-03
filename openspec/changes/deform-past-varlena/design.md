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
of a cursor stores the kept columns it passes: a row with NULLs stores
its NULLs, by its bitmap, and a row shorter than the column stops at
its last attribute, so no column the row lacks lies behind its cursor.

A request chooses its path once: a request for a kept column, or one
that walks past a kept column, takes the path of kept columns; any
other takes the plain path, the code it ran before the change. A row's
choice is a test of its cursor.

Refused: deforming every column of the scan's targets for every row at
its first request. A filter that drops most rows on its first clause
would pay for the columns of rows it drops; a kept value is a value the
walk passes anyway.

Refused: the test of a kept column in the plain path. The machine code
of a value's deform grew by five instructions against the base's, on
every value of a scan that keeps nothing.

### Which columns are kept: the walks they save

A column is kept when the walks it saves cost more than its stores.
Two counts per column, over a window of batches:

- **reached**: the rows a walk carried to the column or past it. A
  row's cursor stands past a column when the row was read for the
  column or for a later one, so this is the popcount of the union of
  the masks of the column and of every later column, taken when the
  batch is reset, from the last column back.
- **behind**: the rows read for the column after a read of a later
  column had carried their cursor past it. Unless the column is kept,
  each of them walks again from the tuple's start. They are counted at
  each request: the requested rows that the masks of the later columns
  hold.

Neither count looks at a row: a few operations a column, a batch or a
request, and no work a value. They are taken on one batch in four, so
that a scan that keeps nothing pays for them on a quarter of its
batches. A scan that has asked for one column past the cached offsets
only takes no counts: it has nothing to keep.

Keeping a column costs a store on each reached row. Not keeping it
costs, on each behind row, a walk from the cached offsets to the
column, one step an attribute. So when the window reaches 1024 rows
counted, about 4096 read, each column with `reached` above zero is
kept for the next window if

```text
behind × distance × 2 ≥ reached
```

where `distance` is the number of attributes from the first one past
the cached offsets to the column, both counted, and the factor 2 says
that a store costs about half a step of the walk. The counts then
start again. A column no walk reached keeps its flag. The first window
keeps nothing, so a scan shorter than a window runs as before. The
window and its decision follow the key filter's in `nodes/filter.c`,
with a constant of its own: the runtime does not include the nodes.

On TPC-H's `lineitem` the cached offsets end at attribute 4. Q01's
aggregate reads attributes 5 to 10 behind the cursor on 98 % of the
rows: kept, and its 5.9 M walks from the start fall to 4 000. Q12 reads
`l_commitdate` (distance 8) behind the cursor on 29 % of the rows and
`l_shipdate` (distance 7) on 15 %: kept, 2.8 M walks fall to 2 000. Q06
reads `l_discount` (distance 3) on 15 %: 0.9, not kept. Q14 reads its
aggregate's columns on 1.3 %: not kept. A filter's own column is read
on every row it is reached, but never behind the cursor: not kept, so
its request takes the plain path.

Refused, each measured on Q01, Q06, Q12 and Q14:

- The share of all reads of a column, not of those behind the cursor.
  A filter's own column came out at 1 and was kept, and its request,
  on every row, went through the path of kept columns: Q06 took 4 to
  8 % longer.
- One share for every column, without the distance. At a half, Q12's
  columns stayed unkept; at an eighth, Q06's `l_discount` was kept,
  three steps from the cached offsets, for no gain.
- Counting `reached` from the rows' cursors, a row at a time: the
  increments of one counter wait for each other, and the reset took
  3.1 % of Q14's profile against 0.4 % before. The masks give the same
  count, short rows aside, with no work a row.
- The counts on every batch. Q03, Q08 and Q21, which keep nothing,
  took 2 to 7 % longer; counted on one batch in four they run as
  before.

### The cost of a value of varying length

The walk's step for a value of varying length was `align_fetch_then_add`
of PostgreSQL: it checks for a padding byte, for an external value and
for a four-byte header, 17 instructions of machine code a value in the
walk. Every short value, such as each `numeric` and `char(1)` of
`lineitem`, has a one-byte header that needs none of them. A forward
walk to a column past the cached offsets goes through the batch's own
loop, the one that keeps columns, with a step that reads a one-byte
header first and leaves any other value to the general step. The back
cursor and the columns at cached offsets keep `tess_deform_advance`.

Q06 took 4 % less time with it, Q14 the same. The same loop with the
general step was 5 to 17 % slower than with this step, and slower than
`tess_deform_advance`: the gain is the step's, not the loop's.

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

- `runtime/heap_batch.c`: the walk that keeps columns, its step for a
  one-byte header, the counts and the window.
- `docs/runtime.md`: how a batch deforms.

## Tests

- `test/tessera_heap_batch_test.c`: a later column first, then earlier
  ones on most rows, with no restart after the first window; the same
  read on few rows, with restarts and no kept column; rows with NULLs;
  rows shorter than the column; a reset batch.
- `test/sql/heap_batch.sql`: the C test over a table with NULLs and
  short rows.
- `test/sql/filter.sql`: a filter and an aggregate over a table of
  `lineitem`'s layout, `Restarted Datums` those of the first window
  when the aggregate reads most rows, every read when it reads 1 %.
