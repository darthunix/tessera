## Why

The Parquet node reads a column a window of up to 1 024 rows at a time
and gives it to consumers in batches of up to 64 rows. For each batch
it asks the reader, `tess_parquet_values`, to make the values of the
batch's rows from the window, through `ReadPlan::read` of
`datum-arrow`. For a plain column of integers, floating-point numbers,
days, times or booleans, that read's loop, `dense` or `dense_bits`,
converts every slot the window holds, asked or not, a word of 64 slots
at a time. So each batch converts the whole window: a full window of
16 batches converts each of its values 16 times, and writes 8 KB of
words for 512 bytes asked. Each call also allocates and frees a mask
of the window's `NULL` rows.

A profile of ClickBench's Q3, `SUM`, `COUNT` and `AVG` over two integer
columns, at the end of `parquet-batch-arrays`, taken with the macOS
tool `sample`, which records the running function once a millisecond:
`ReadPlan::read` is on top of 794 of 3 266 samples, 24.3 per cent of
the query (`docs/benchmarks/2026-11-27-parquet-batch-arrays/
profile-q3-end.txt`), and the frees of the mask, by the profile's call
graph, 71 more, 2.2 per cent. Q3 runs in 4.13 times pg_duckdb's
cycles, Q7, `MIN` and `MAX` of a date, in 2.6 times.

The roadmap's entry asked whether the conversion of a value or the call
for each batch costs. Neither, as the profile and the code show: the
conversion of the rows no batch asked costs. A form of integers in
their own width, a change of the C API, of the kernels and of the
nodes, is not needed for it.

This change takes up the roadmap's entry `parquet-integer-columns`.

## What Changes

- `datum-arrow`'s dense loops, `dense` and `dense_bits` of `read.rs`,
  convert the words of the mask from the first that asks a slot to the
  last that asks one, as now, word by word; the words before and after
  keep their places, and their words of `NULL` slots are cleared. A
  batch of 64 rows converts its own word or two, not the window. What
  `ReadPlan::read` promises does not change: it may write the place of
  a slot not asked, and now writes fewer.
- `tess_parquet_values` keeps its mask of `NULL` rows in the reader's
  handle for the next call, sized to the caller's mask as now, and
  allocates it once a reader instead of once a call.
- The wrapper's rows and `ANALYZE` ask every held row of a window:
  their conversions stay as they are, without the allocation a call.
- Results, errors, EXPLAIN and the settings stay as they are.
- Not in this change: the node's copy of a batch's values from the
  window into its arrays, 14 per cent of Q3, and a form of integers in
  their own width; the close puts what the count says of them in the
  roadmap.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

None: what SQL sees and what `datum-arrow`'s API promises do not
change, and `skip_specs` is set. The design of `datum-arrow` says how
its dense loops go over a mask.

## Impact

- Code: `crates/datum-arrow/src/read.rs` (`dense`, `dense_bits`) and
  `crates/tessera-parquet/src/c.rs` (`tess_parquet_values` and the
  reader's handle).
- Tests: `crates/datum-arrow/src/read/tests.rs` and
  `crates/tessera-parquet/src/c/tests.rs`; no SQL suite changes.
- Public headers, settings, SQL suites: none. `datum-arrow` is a crate
  meant for use outside Tessera: its documentation stays true, its
  public API and its snapshot `public-api.txt` do not change, and its
  job of CI checks it alone.
- Contracts: no rule of `datum-arrow`'s or of `parquet-fdw`'s contract
  is touched, and no boundary is crossed: the loops stay inside the
  conversions `datum-arrow` owns, and the mask is the reader's handle's
  own memory, freed with the handle by its memory context's callback,
  as the rule "The reader's memory is the query's" asks. Neither
  forecast names such a change: the close adds an item to each, "how a
  dense loop goes over a mask" to `datum-arrow`'s and "the reader's
  work for a call of a window's values" to `parquet-fdw`'s.
- Roadmap: the entry `parquet-integer-columns` leaves it at the close.
