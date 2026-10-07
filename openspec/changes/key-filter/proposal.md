## Why

A hash join may hand its Bloom filter to the node below it, which then
drops the rows that cannot find a pair before its costlier clauses run.
This contract between two nodes is described only in the comments of
`tessera/node.h` and `tessera/runtime_input.h` and in `docs/nodes.md`,
and no requirement ties it to a test. Describing the Bloom filter in
the capability `hash-table` found that the texts and the code disagree:

- EXPLAIN shows `Rows Removed by Bloom Filter` on the join and on the
  TessFilter below it, and the two count different rows: the join only
  rows with a key, the TessFilter also the rows it drops for a NULL key.
  In the join suite, 9144 rows where the filter rejected 9048.
- `node.h` says that everything of a filter stays valid until the
  parent takes it back; TessAppend hands its children the key columns
  from an array of its stack, and the join the filter's struct. It
  works because TessFilter copies them.
- A RIGHT join hands its filter down too, as the comments and the guide
  do not say, and no test shows it.
- A participant that leaves a shared table does not take the filter
  back from its child, and another participant frees the filter's words
  later; no batch is read in between, so nothing reads them.
- TessFilter, asked to take a second filter it refuses, overwrites the
  keys of the one it holds.
- `docs/nodes.md` leaves out that a join that spills keeps its filter,
  says that the filter checks the rows the batch clauses kept (only the
  batch clauses before the first row-wise one run first), and that the
  core's `Rows Removed by Filter` counts two parts of TessFilter's
  where it counts three.

## What Changes

- A new capability `key-filter`: a key filter and its call, who hands a
  filter down, who takes it, what the child removes, and the rows a
  filter removes as EXPLAIN shows them. Five requirements; every
  scenario names its test.
- The text about the key filter moves from `docs/nodes.md` to
  `openspec/specs/key-filter/design.md`, and is then written anew.
- Corrections, each with its test:
  - TessFilter's `Rows Removed by Bloom Filter` counts only the rows
    with a key that the filter rejected, as the join's does, and a new
    line, `Rows Removed by NULL Key`, the rows it drops for a NULL key;
    `cargo tpch` counts the new line among TessFilter's;
  - the contract of `TessKeyFilter` says what parents do: the struct and
    its arrays for the call only, the words until taken back;
  - a participant takes the filter back before it leaves a shared
    table;
  - TessFilter refuses a filter without touching the one it holds;
  - the comments and the guides follow the code.
- Tests of the promises without one: RIGHT joins, refusals, a spilling
  table and a hashed key, a filter too small.

## Capabilities

### New Capabilities
- `key-filter`: the filter of keys a parent node hands its child, and
  what EXPLAIN shows of it.

### Modified Capabilities

## Impact

- C API: `TessKeyFilter` and `set_key_filter` in `tessera/node.h`,
  `tess_input_set_key_filter` in `tessera/runtime_input.h`: their
  comments; no type or function changes.
- EXPLAIN: TessFilter shows `Rows Removed by NULL Key`, and its
  `Rows Removed by Bloom Filter` is smaller by those rows. Expected
  outputs of the join suite change.
- Code: `nodes/filter.c`, `nodes/hashjoin_shared.c`,
  `tools/tessera-tpch/src/participation.rs`.
- Documents: `docs/nodes.md`, `docs/node.md`, `docs/runtime.md`.
- Tests: `test/sql/join.sql`, `test/sql/unary.sql` with
  `test/tessera_unary_test.c`.
