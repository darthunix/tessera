## Why

The C API of the hash table has two parts that no requirement ties to
a test yet: how the participants of a parallel join build one table
together, phase by phase, and go over the partitions on disk round by
round (`tess_build_*`, `tess_round_step`), and how a table that spills
decides which partitions it splits into and which go to disk
(`tess_table_spill_*`). Their text is in `docs/table.md`,
`docs/spill.md` and the comments of `tessera/table.h`. Comparing them
with the code found:

- A join that spills hangs, and can be neither cancelled nor
  terminated, when `tessera.join_spill_spilled_weight` is above 0: the
  rule chooses again a partition already on disk that holds the most,
  and the join's own spill demotes it again, which frees nothing. A
  shared table hangs the same way with
  `tessera.join_shared_spill_evictions` at 0. Both were run: the
  backends spun past a `statement_timeout` of 5 s and had to be killed.
- `tess_build_totals` writes the records before it checks its other
  outputs, though every call checks its arguments before it changes
  anything.
- Values out of range pass silently: an attach that answers a phase
  past FREE leaves, a level's bits past 32 wrap in
  `tess_table_spill_splits`, the counts of a build and of the spill
  words wrap past 2^64 - 1, a file number wraps past 2^32 - 1, and words
  are attached for more than 65536 partitions.
- The texts disagree with the code: the diagram of the atomics has no
  FLUSH and no OUTER phase; the NULL columns are an OR, not a sum; no
  spinlock guards the chunks' list; a late participant links and writes
  its share of the outer side, it does not only wait; a late
  participant of a round probes; the phases are the table's C API, not
  the join's; four counter words, not three; eight negative models,
  not four; `tess_table_spill_evict` returns a partition already on
  disk again, which its comment denies; the rule of records sends one
  partition a call, not each.
- No test shows: a late participant at FLUSH, SIZE and LINK; a round's
  participant at ALLOCATE and PROBE; an unknown state through
  `tess_round_step`; the NULL bits as an OR; every counter cleared;
  chunk numbers and duplicates under several participants; misaligned
  counters; the budget exactly met; the words sized at 0 and 65537
  partitions; weights that are not finite.

## What Changes

- The capability `hash-table` gets its last part: the counters and the
  phases of a shared build, the rounds over a partition on disk, the
  words of a spill, a split, the rule that sends partitions to disk,
  the records, starts and files of the rounds, and a partition read
  back that splits; the requirement on calls at the same time names
  what else changes atomically. Nine requirements added and one
  modified; every scenario names its test.
- The text of `docs/table.md` on several participants and the atomics
  moves to `openspec/specs/hash-table/design.md`, and is then written
  anew.
- Corrections, each with its test:
  - a join frees what it holds of a partition the rule chooses again,
    and the loops of its eviction check for interrupts;
  - `tess_build_totals` checks every output before it writes one;
  - an attach past the last phase, bits past 32, counts past their
    range and words for more than 65536 partitions are refused;
  - the comments and the guides follow the code.
- Tests of the promises without one, in Rust, in the C entry points and
  in loom.

## Capabilities

### New Capabilities

### Modified Capabilities
- `hash-table`: the shared build, its rounds and the words of a spill.

## Impact

- C API: `tessera/table.h`: the comments of `tess_build_*`,
  `tess_round_step` and `tess_table_spill_*`; calls now refuse what
  they took before (a phase past FREE, bits past 32, counts past their
  range, more than 65536 partitions); no type changes.
- Behavior from SQL: a join that spills ends with every value of
  `tessera.join_spill_spilled_weight` and
  `tessera.join_shared_spill_evictions`.
- Code: `crates/tessera-kernels/src/table/phases.rs`,
  `crates/tessera-kernels/src/table/shared_spill.rs`, the C entry points
  in `crates/tessera-capi`, `nodes/hashjoin_spill.c`,
  `nodes/hashjoin_shared.c`, `nodes/hashjoin.h`.
- Documents: `docs/table.md`, `docs/spill.md`.
- Tests: the Rust tests of the table, `loom.rs`, the C entry points'
  tests, `test/sql/join.sql`.
- Roadmap: takes up `hash-table-participants`; the aggregate states and
  the items of a sort go to the capabilities of the grouping and the
  sort, and the plan of a spill's partitions to `spill-format`.
