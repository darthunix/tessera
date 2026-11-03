## Why

A heap batch deforms a row's columns on request: a cursor walks the
tuple from attribute to attribute up to the one asked for and keeps
only that value. The columns TPC-H's `lineitem` queries read lie past
four numerics and two `char(1)`, all of varying length, so each walk
reads six length headers, and a column before the cursor walks from the
tuple's start again, behind the second cursor of a row.

Profiles of TPC-H at SF 1, serial, with Tessera on (the analysis of
2026-11-03): `deform` is 26 % of Q01, 50 % of Q06 and Q14 and 52 % of
Q12. Q01 reads `l_shipdate`, attribute 11, in its filter, then
attributes 5 to 10 in TessAgg: 41.5 M values deformed for 6 M rows, 5.9
M of them walks from the start again, seven walks a row where one would
do. Q12 has 2.8 M such walks of 10.5 M values, Q06 1.2 M of 7.3 M.

A prototype that keeps, on a walk, the values of the columns the scan's
consumers asked for before ran Q01 in 565 and 560 ms against the base's
704 and 711, Q12 in 240 and 222 against 258 and 266, base and prototype
in turn, the answers the same. Q14 took 2 to 3 % longer and Q06 up to
5 %: their filters drop most rows, and the prototype stored on every
walk the columns their aggregates read on the few rows kept.

## What Changes

- A walk of a row forward to a column keeps the values of the columns
  the scan keeps, as it passes them; their requests find them stored
  and walk nothing. A column is kept when, over a window of 4096 rows,
  its consumers read it on at least half of the rows a walk carried
  past it: Q01's aggregate columns (0.98) are kept, Q14's (0.013) are
  not, so Q14 stores nothing more. The counts are taken once a batch
  from the batch's masks and the rows' cursors, nothing a value. Rows
  with NULLs and rows shorter than the column keep them too.
- The walk's cost a column of varying length is measured on the long
  walks of Q06 and Q14, and a path for one-byte headers is tried; it is
  kept only if it gains there.
- `Deformed Datums` and `Restarted Datums` keep their meaning: a value
  stored on a walk counts as deformed when a consumer reads it.
- Tests: the heap batch's test of a walk that keeps columns (a later
  column first, then earlier ones on most rows, no restart after the
  first window; the same on few rows, nothing kept; rows with NULLs;
  short rows; a reset batch), and a filter and an aggregate over
  `lineitem`'s layout whose `Restarted Datums` fall near zero.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

None: results, plans and the C API stay; `heap_deform.h` is not
changed. The change declares `skip_specs: true`.

## Impact

- Code: `runtime/heap_batch.c`.
- Tests: `test/tessera_heap_batch_test.c`, `test/sql/heap_batch.sql`.
- Documents: `docs/runtime.md`.
- Runs: TPC-H Q01, Q03, Q06, Q10, Q12, Q14, Q19 and Q21 serial and with
  two workers, the family `win`, which reads one column a filter, and
  profiles of Q01 and Q14.
