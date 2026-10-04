## Why

What a spilled block looks like on disk is written in `docs/spill.md`
and in comments of the code, and they have drifted apart. The document's
diagram of the header and five comments in three files name two kinds of
block; the code has had three since the join's outer rows went to disk
by column. The document says the write buffer is 32 to 256 kB; the
macro's floor is 8 kB. Nothing ties a statement about the format to a
test, so nothing notices when one stops being true.

The format is the first part of the system to be described as an
OpenSpec capability. It is small, stands on its own, and has tests with
names.

## What Changes

- A new capability `spill-format`: the header of a block, the three
  kinds of body, the packed forms of records and of columns, the checks
  of a reader, and the sets of files that hold the blocks. Every
  scenario names the test that shows it, or says that none does.
- The text about the format moves from `docs/spill.md` to
  `openspec/specs/spill-format/design.md`. The nodes' policies
  (partitions, eviction, the weights) stay in `docs/spill.md` until the
  join and the grouping are described.
- The definitions (the header's fields, the kinds, the codes of the
  packed forms, the trailer of a shared file) live in the spec only; the
  design keeps the reasons.
- No code, comment or test changes. What the comparison of document,
  comments, code and tests found is listed for the maintainer in
  `outcome.md`.

## Capabilities

### New Capabilities
- `spill-format`: the format of spilled blocks and of the files that
  hold them, and the checks of a reader.

### Modified Capabilities

## Impact

- Documents: `docs/spill.md` (three sections move), `README.md` and
  `docs/nodes.md` keep their links to `docs/spill.md`.
- Public headers described: `include/tessera/spill.h`,
  `include/tessera/runtime_spill.h`. Not changed.
- Tests named by the scenarios:
  `crates/tessera-spill/src/{lib,pack,columns}.rs`,
  `crates/tessera-kernels/src/spill_columns.rs`,
  `crates/tessera-capi/tests/{spill,table}.rs`, `test/sql/spill.sql`,
  `test/sql/table.sql`.
