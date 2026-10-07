## Why

The capability `hash-table` describes the table itself. What the join
and the grouping build on it is still described only in `docs/table.md`
and in the comments of `include/tessera/table.h`: partitions for
spilling, the Bloom filter of the keys, and the marks of RIGHT and FULL
joins. Nineteen calls of the C API have no requirement, and the texts,
the code and the tests disagree:

- An append checks the table's index when there is one, as the spec of
  `hash-table` says; the append by partition does not.
- `tess_table_split` with a capacity of 0 copies nothing and answers as
  it does at the end of its chunk, so a caller would stop too early.
- The marks accept a record size that is not a multiple of 8, which
  gives references that name no record, and a record size other than
  the table's, which makes a mark write past the caller's words. A
  cursor past its chunk's used mark is taken, and the walk skips the
  rest of that chunk without an error. `tess_table_mark` takes more than
  32768 chunks.
- Three texts say that a filter of 16 bits a record lets about one
  absent key in a hundred through; no test measures it.
- `table.h` leaves out the limit of 64 columns of an append by
  partition, the bound of the shift, the checks of the chunk numbers,
  and most of how a merge treats a state. It says that a spilling join
  has freed the index before the walk of the marks, which no join does,
  and that the cursor moves past the last record looked at, where it
  stops on the first record not given. `status.h` calls status 2 an
  int4 result that does not fit, where a merge gives it for int8.
- None of the eleven entry points of the Bloom filter has a Rust test
  of its own, so a mutant of their wrappers lives. Filling a filter
  together and marking together are not in the loom model.

## What Changes

- Ten requirements added to `hash-table`: partitions of a table,
  appending rows by partition, splitting a chunk by partition, groups by
  partition, merging groups read back, a Bloom filter of the keys, the
  size of a filter, a shared filter, a filter filled together, and the
  marks of RIGHT and FULL joins. Every scenario names its test.
- The text about partitions and the filter moves from `docs/table.md`
  to `openspec/specs/hash-table/design.md`, and the design gets
  sections on partitions, the filter and the marks, written anew.
- Corrections of the code where it breaks its own promise, each with
  its test:
  - the append by partition checks the table's index when there is one;
  - a split copies at least one record a call;
  - the marks refuse a record size that is not a record's, or not the
    table's when the table has an index, a cursor past its chunk's used
    mark, and more than 32768 chunks;
  - the loom model fills a filter together and marks together, with a
    negative test of each;
  - the comments and the documents follow the code.
- Tests for the promises without one.

## Capabilities

### New Capabilities

### Modified Capabilities
- `hash-table`: partitions, the Bloom filter and the marks, added to
  the table itself.

## Impact

- Documents: `docs/table.md` keeps the phases of a shared build and of
  its rounds, the aggregate states and the items of a sort.
- Public headers described: `include/tessera/table.h`,
  `include/tessera/status.h`. The C API keeps its functions and types;
  a few calls refuse what they accepted: a split of no records, an
  append by partition or a mark of another record size than the
  table's, a record size that no record has, a cursor past its chunk.
- Code: `crates/tessera-kernels/src/table/{batch,bloom,marks,loom}.rs`,
  `crates/tessera-capi/src/c/table.rs`.
- Tests named by the scenarios: `crates/tessera-kernels/tests/table.rs`,
  `crates/tessera-kernels/src/table/{mod,marks,loom}.rs`,
  `crates/tessera-capi/tests/table.rs`, `test/sql/table.sql` with
  `test/tessera_table_test.c`.
- No setting, plan or SQL result changes.
