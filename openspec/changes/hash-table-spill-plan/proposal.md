## Why

Two calls of `tessera/spill.h` plan a level of a table that spills:
`tess_spill_partitions` gives the number of its partitions and
`tess_spill_chunk_len` the length of its chunks. The grouping, the join
and the shared table of a join all plan by them, with parameters of
their own. No capability describes them: `spill-format` leaves them to
the nodes, and `hash-table` did not take them. So nothing ties their
promises to tests. Comparing their comments with the code found:

- a `shift` past which even the least partitions do not fit in the 32
  bits of the hash is taken: the partitions above the hash's bits stay
  empty;
- expected bytes below zero or not a number are taken as none;
- a `min_chunk` that is not a multiple of 8 gives a chunk shorter than
  it, since the length is rounded down after the bound, and one below 8
  can give a chunk of no bytes;
- no test shows the rule on an example; one test compares it with the
  nodes' former rules, and one its refusals of bounds out of order.

## What Changes

- The capability `hash-table` gets two requirements: the partitions of
  a level and the length of its chunks. Every scenario names its test.
- The design of `hash-table` explains the plan of a level, with an
  example, and `spill-format`'s design points to it.
- The calls refuse a `shift` the least partitions do not fit after,
  expected bytes below zero or not a number, and a `min_chunk` below 8
  or not a multiple of 8, each with its test.
- Tests of the rule on examples.

## Capabilities

### New Capabilities

### Modified Capabilities
- `hash-table`: the plan of a level of a table that spills.

## Impact

- C API: `tessera/spill.h`: the comments of `tess_spill_partitions` and
  `tess_spill_chunk_len`; the calls refuse what they took before; no
  type changes.
- Code: `crates/tessera-spill/src/plan.rs`.
- Documents: the designs of `hash-table` and `spill-format`.
- Tests: the unit tests of `plan.rs`, the entry points' tests.
- Roadmap: the finding of the plan's calls leaves "Not placed".
