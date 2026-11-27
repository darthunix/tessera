## Why

The Parquet node, `TessParquetScan`, makes each batch of up to 64 rows
in a memory context it resets at the next batch. Into it, at each
batch, it allocates and zeroes three arrays with an entry for each
attribute of the foreign table, the values, the `NULL` flags and the
rows made: 105 entries each for ClickBench's `hits`, 2 520 bytes,
whatever columns the query reads. Then, for each column a consumer
asks for, it allocates and zeroes an array of the batch's values and
one of its flags, 576 bytes for a full batch. The first string column
asked in the bytes form adds another array of 105 entries, and each
such column a view and four arrays of the batch's rows.

Profiles of ClickBench's queries at the end of `parquet-string-views`,
taken with the macOS tool `sample`, which records the running function
once a millisecond, put these allocations and their zeroing at about
half of Q1, `SELECT COUNT(*) FROM hits`, which reads no column, and at
a tenth of Q3, `SUM`, `COUNT` and `AVG` over two integer columns. The
100 million rows of `hits` are 1.56 million batches. In the last
count, of `agg-date-min-max`, Q1 ran in 1.58 times pg_duckdb's cycles
and Q3 in 4.43 times.

This change takes up the roadmap's entry `parquet-batch-arrays`.

## What Changes

- The node allocates the arrays of a batch once a scan, with an entry
  for each column the scan reads, not for each attribute of the table.
  A column's arrays of 64 rows, the most a batch holds, are allocated,
  zeroed, at its first ask in the scan, and serve every batch after it.
- At each batch, and at a rescan, the node clears, for the scan's
  columns alone, the rows made in both forms and a view's dictionary: a
  few words a column. A query that reads no column clears nothing and
  allocates nothing.
- The rows a batch does not make keep what an earlier batch wrote:
  written memory of no meaning, which the batch's API allows and the
  heap's provider already leaves. The rows a batch makes, and the
  values, flags and bytes a consumer gets for the rows it asks, are
  the same as now; an asked `NULL` row of the bytes form gets zeros,
  as the zeroed arrays gave it.
- Under assertions, the node wipes the values and the bytes' places of
  its arrays at each batch, so that a consumer that keeps a column past
  the batch's release reads the wipe in the suites where the next batch
  has not made the row again.
- The memory reset at each batch keeps only what lives for one batch:
  the arenas of strings made as values, the references that keep a
  window's bytes, and the scratch of the checks under assertions.
- Results, errors, EXPLAIN and the settings stay as they are.
- Not in this change: how a column's values reach the batch, which the
  entry `parquet-integer-columns` holds.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

None: what SQL sees does not change, and `skip_specs` is set. The
design of `parquet-fdw` says how the node keeps a batch's values.

## Impact

- Code: `parquet/node.c`, the node's batch (`next_batch`,
  `batch_column`, `batch_bytes`, `make_bytes`, `node_begin`).
- Tests: the test module's consumers of a batch,
  `test/tessera_parquet_test.c` (`parquet_test_asks` and
  `parquet_test_bytes`, which gain an optional argument), and the
  suites `test/sql/parquet.sql` and `test/sql/parquet_cancel.sql`, with
  new queries and their expected output.
- Public headers, settings, other suites: none.
- Contracts: the change keeps to the node, one part of `parquet-fdw`,
  and crosses no boundary: the arrays are the node's, in the query's
  memory, and a consumer holds them until the batch's release, as now.
  No item of the forecast names such a change; the close adds one,
  "how the node keeps a batch's values", and the pull request compares
  what it touched under Architecture. The rule "The reader's memory is
  the query's" holds.
- Roadmap: the entry `parquet-batch-arrays` leaves it at the close.
