## Why

A hash table of a join or a grouping lies in chunks of memory: the
first one small, 64 kB, so that a table of a few rows takes little
memory, and the later ones an eighth of the node's memory limit
(`hash_mem`), but at most 1 MiB. The design of `hash-table` states
this rule once. The code writes it in several places, and the copies
have drifted. Each figure below follows from the sizes of
`relhash` (a record of two keys is 32 bytes; an index is 96 bytes and
4 bytes a bucket, its buckets twice its capacity rounded up to a
power of two, at least 1024):

- **A DISTINCT set ignores the limit.** The table that keeps the pairs
  of a `count(DISTINCT x)` takes 1 MiB for every chunk past the first,
  whatever `hash_mem` is. At `hash_mem` = 256 kB, a set of 3000
  distinct pairs takes 1 146 976 bytes: chunks of 64 kB and 1 MiB and
  an index of 32 864. The rule gives two chunks of 64 kB: 163 936.
- **A table's growth is missed by the node's peak.** EXPLAIN's Memory
  Usage is the node's peak. A DISTINCT set notes neither a new chunk
  nor a regrow, when the old index and the new one are both alive, nor
  its first index when the input is empty. A grouping that reads a
  partition back merges chunks without counting them, starts its
  merge from a total of bytes it did not reset, ends it with the size
  of an index that has since grown, and frees an old index before it
  notes the new one.
- **A serial join forgets its index.** Before each batch it spills if
  its chunks, its values and two more chunks pass `hash_mem`; its
  index comes after the last batch. At `hash_mem` = 16 MiB, a build of
  420 000 records of 32 bytes passes that check with 13 697 024 bytes
  of chunks, then adds an index of 4 194 400: 17 891 424, above
  16 MiB. A shared join already reserves the index.
- **The first index of a grouping can pass its share.** Its capacity
  is capped at `hash_mem / 32` groups so that its buckets take a
  quarter of `hash_mem`, but the buckets round up to a power of two:
  at `hash_mem` = 3 MiB they take 1 MiB, a third.
- **A merge of groups read back skips a check.** It adds chunks to the
  table without the check of the most chunks, 32768, that an insert
  makes before it raises SQLSTATE 54000.

The rule already has an owner that no table in memory uses:
`tess_spill_chunk_len` (Rust, `tessera_spill::plan::chunk_len`), the
length of a level's chunks, which the spill's levels take. For one
partition at a share of 8, between 64 kB and 1 MiB, it gives the rule
of the design, for every limit.

This change takes up the roadmap entry `table-growth-one-owner`.

## What Changes

- Each fault above is first shown by a test that fails, then fixed;
  the last, which takes hundreds of MiB of chunks to reach, is shown
  by review, and the helper it goes through by its own test.
- Every table in memory takes the length of its chunks past the first
  from `tess_spill_chunk_len`: the serial join's records and values,
  the shared join's participants, the grouping and the DISTINCT sets.
  C keeps no formula of its own.
- One internal helper of `nodes/` keeps a table's list of chunks and
  raises 54000 before it would pass the most chunks, and allocates and
  initializes a chunk in a memory context. The nodes call it instead
  of their copies; the shared join keeps its own allocation in DSA.
- One note of a table's growth, which counts a replaced index while it
  is alive, used by every path that adds a chunk or rebuilds an index.
- The serial join checks, before a batch, the index its records will
  need, by its exact size, without counting it as memory taken. A
  build that fits with its index but leaves less than the room the
  join keeps for two more chunks now spills too; the measurement gives
  its cost.
- A grouping's first index is capped by its buckets.
- No **BREAKING** change. Visible from SQL: EXPLAIN's Memory Usage of
  groupings with DISTINCT and of groupings that read partitions back;
  a serial join near `hash_mem`, which may spill where it went past
  the limit; SQLSTATE 54000 from a merge at the most chunks.

## Capabilities

### New Capabilities

- `table-memory`: the memory of a node's hash table in memory, seen
  from SQL: the length of its chunks, what EXPLAIN's Memory Usage
  counts, the serial build's index within `hash_mem`, the first index
  of a grouping, and the error at the most chunks. The Purpose of
  `hash-table` leaves this to the node's own capability; the join's
  and the grouping's have no spec yet, and this one covers what they
  share.

### Modified Capabilities

None.

## Impact

- Code: `nodes/hashjoin.c`, `nodes/hashjoin.h`,
  `nodes/hashjoin_shared.c`, `nodes/hashjoin_spill.c`,
  `nodes/agg_group.c`, `nodes/agg_keydict.c`, `nodes/agg_spill.c`,
  `nodes/agg.h`, `nodes/agg_node.h`, a new `nodes/table_chunks.c` and
  `.h`, and the build of `nodes/`.
- Rust: none in the rule; tests of `tess_spill_chunk_len` for one
  partition.
- Tests: the SQL suites `agg` and `join`, with cases whose Memory
  Usage is exact (a table in memory with integer keys: the node counts
  its own chunk and index lengths) or bounded from below (a spill);
  an invariant of the node's count, checked in assert builds; the C
  test module for the helper's directory.
- Documents: the new capability's spec and design;
  `openspec/specs/hash-table/design.md`; `docs/nodes.md` on Memory
  Usage; task 3.9 of the change `spill-crate`, which moves the rule
  into `relspill` with its callers.
- Benchmarks: cases of `join`, `spill` and `anyagg` named in the
  design, added first; `bench/pg/run.sh` keeps the `PGOPTIONS` it is
  given, so that a run can bound its statements.
- Settings, formats, the C API of the kernels: unchanged.
