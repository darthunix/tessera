## Why

The hash table that joins, groupings and sorts keep their rows in is
described in `docs/table.md`, in the comments of
`include/tessera/table.h` and in the comments of the Rust code. They
have drifted from the code and from each other:

- `table.h` says that `tess_table_append_columns` takes at most 64
  columns and one word of NULL bits; the code takes 2048 columns and a
  word of NULL bits for every 64.
- Three texts say that every call checks the whole header; append never
  reads one, and a reserved word of the header is never checked.
- Three texts say that a damaged table is an error, "never a hang". A
  walk down a chain is bounded only by the count of records in the
  header, which nothing checks: with a damaged count, a chain with a
  loop is walked practically for ever.
- `kernels.h` promises that a call writes nothing when it fails. Five
  entry points do their work, and then refuse a missing output pointer.
- A comment calls the record a probe finds "the newest" of its key; in
  a grouped chain it is the oldest.
- `tess_table_create` accepts a record larger than a chunk, though no
  such record can ever be appended.

No statement about the table is tied to a test, so nothing notices when
one stops being true.

## What Changes

- A new capability `hash-table`: the index and its header, chunks and
  references, a record, the errors of a call, appending, linking,
  looking rows up, the records of a key together, the calls of one
  writer, and the calls that may run at the same time. Ten
  requirements; every scenario names its test.
- The text about the table itself moves from `docs/table.md` to
  `openspec/specs/hash-table/design.md` and is then written anew, from
  the whole to the details. What the other parts of the system build on
  the table stays in `docs/table.md` for now: partitions for spilling,
  the Bloom filter, the marks of RIGHT and FULL joins, aggregate states,
  the items of a sort, and the phases of a shared build.
- Tests for the promises without one, and corrections of the code where
  it breaks its own promise, each with its test:
  - a walk is bounded by the room of the chunks as well as by the count;
  - the entry points check their outputs before they change anything;
  - a record larger than a chunk is refused when the table is sized;
  - the reserved word of the header is checked;
  - a walk by `tess_table_scan` takes at least one record a call, so
    that a count of 0 always means the end;
  - the comments that disagree with the code follow it.

## Capabilities

### New Capabilities
- `hash-table`: the format of a table in memory, local or shared, and
  the calls over it.

### Modified Capabilities

## Impact

- Documents: `docs/table.md` (its sections about the table itself
  move); links to it from `README.md`, `docs/` and the code stay.
- Public headers described: `include/tessera/table.h`,
  `include/tessera/table_key.h`. The C API keeps its functions and
  types; a few calls refuse what they accepted: a record larger than a
  chunk, a walk of 0 records, a missing output given with work to do.
- Code: `crates/tessera-kernels/src/table/`,
  `crates/tessera-capi/src/c/table.rs`.
- Tests named by the scenarios: `crates/tessera-kernels/tests/table.rs`,
  `crates/tessera-kernels/src/table/{mod,loom}.rs`,
  `crates/tessera-capi/tests/table.rs`, `test/sql/table.sql` with
  `test/tessera_table_test.c`.
- No setting, plan or SQL result changes. The capability `spill-format`
  keeps the fingerprint of a table's layout.
