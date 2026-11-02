## Why

A heap batch deforms a row's columns on request with a cursor that
walks the tuple from attribute to attribute and resumes where it
stopped. A column before the cursor starts a walk from the tuple's
start with a local cursor, and the batch throws that cursor away after
the one value. Columns read in ascending order behind the cursor so
walk the tuple from its start once each.

TPC-H at SF 1 shows what it costs (EXPLAIN VERBOSE, serial). Q1 reads
`l_shipdate`, attribute 11, in its filter, and then attributes 5 to 10
in TessAgg: 35.5 M of its 41.5 M deformed values are restarts, about 45
steps a row where 10 would do. Q21 compares `l_receiptdate >
l_commitdate` and reads attribute 13 before 12: 6.0 M restarts of 16.0
M. Q12 has 4.5 M of 10.5 M, Q6 1.3 M, Q19 1.1 M. `deform` is 29 % to 45 %
of the profiles of Q1, Q14, Q19 and Q21.

## What Changes

- Each row of a heap batch keeps a second cursor, the cursor of its last
  restart. A column before the row's cursor continues it when it lies
  before the column, and starts it from the tuple's start when it has
  passed the column too. The common path of `deform` stays as it was;
  the second cursor is set at a row's first restart, so appending a row
  stores nothing more.
- An expression that compares two columns reads the one before in the
  tuple first; the other then goes on with the row's walk.
- `Restarted Datums` keeps its meaning: a value read by walking the
  tuple from its start again.
- Tests: the heap batch's own test of the second cursor (columns behind
  the cursor in ascending order, one the second cursor has passed, a
  reset batch) and a filter comparing two columns with no restart.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

None: results, plans and the C API stay. The change declares
`skip_specs: true`.

## Impact

- Code: `runtime/heap_batch.c`, `runtime/expr.c`.
- Tests: `test/tessera_heap_batch_test.c`, `test/sql/heap_batch.sql`,
  `test/sql/filter.sql`.
- Documents: `docs/runtime.md`.
- Runs: TPC-H Q1, Q6, Q12, Q14, Q21 serial, and the family `win`, which
  reads one column a filter, the common path.
