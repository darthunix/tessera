## Context

See proposal.md, "Why", for the faults and the motivation.

A table of `relhash` lives in memory its caller owns: an index of
buckets, and chunks that hold the records. The node allocates each
chunk in a PostgreSQL memory context (or in DSA for a shared join),
asks the kernels to initialize it, and lists it in the table's
directory, the arrays of the chunks' bases and lengths. The index is
sized for a capacity of records and rebuilt larger, a regrow, when
half its buckets are taken. A node counts the bytes of its table and
keeps their peak, which EXPLAIN shows as Memory Usage, and compares
its bytes with its memory limit, `hash_mem`, to decide when to spill.

Today each node does this itself:

```
  serial join     hashjoin.c        own length rule, directory in the
                                    query context, chunks in the table
                                    context, 54000; spills by chunks +
                                    values + 2 chunks; index after
  shared join     hashjoin_shared.c own length rule (the join's), DSA,
                                    54000, reserves the index per chunk
  join spill      hashjoin_spill.c  level plan in Rust; 54000 at insert
                                    and at a loaded chunk
  grouping        agg_group.c       own length rule, directory in the
                                    table context, 54000; regrow noted
  DISTINCT set    agg_keydict.c     1 MiB always, own directory, 54000;
                                    nothing noted
  grouping spill  agg_spill.c       level plan in Rust; slots 0 and 1
                                    reserved (source, empty); insert
                                    checks 54000, merge does not; merge
                                    and rebuild keep stale totals
```

The design of `hash-table` states the rule of the chunks once: 64 kB
first, then an eighth of the limit, at most 1 MiB. Only the spill's
levels compute it in Rust; the tables in memory compute it in C, and
one of them does not compute it at all.

## Goals / Non-Goals

**Goals:**

- One owner of the length of a chunk past the first, for every table
  in memory: `tess_spill_chunk_len`.
- One C helper, internal to `nodes/`, for a table's directory and for
  the allocation of a chunk in a memory context, with 54000 before a
  directory would pass the most chunks.
- One note of a table's growth that a regrow cannot slip past.
- The faults of proposal.md fixed, each shown first by a test that
  fails.

**Non-Goals:**

- A spill for a grouping with a DISTINCT aggregate. It never spills by
  design; the planner admits it when its estimate of the pairs'
  records fits `hash_mem`. The estimate leaves out the set's index and
  the room left in its last chunk: smaller chunks remove the 1 MiB of
  slack, but the set may still pass the estimate by its index. That
  gap of the planner is a finding for the roadmap, not fixed here.
- The key dictionary of a grouping (`agg_keydict.c`, PostgreSQL's
  simplehash), another table with another allocator.
- The chunks of `TessRows` (`runtime/rows.c`), which take their length
  from their caller, a sort under `work_mem`.
- The limit of a level below a spill, and the count of block lists,
  marks and buffers: tasks 3.24 and 3.23 of the change `spill-crate`.
- Moving the rule into `relspill`: task 3.9 of `spill-crate`, which
  moves `plan::chunk_len` with every caller this change gives it.

## Decisions

### 1. The length of a chunk comes from the level plan

- **Was:** `join_chunk_len_for` in `hashjoin.c` (the serial join's
  records and values, the shared join's participants), the same
  formula again in `agg_group.c`, and 1 MiB in `agg_keydict.c`.
- **Will be:** a chunk past the first takes
  `tess_spill_chunk_len(limit, 1, 8, 64 kB, 1 MiB)`, the limit being
  `tess_hash_memory_limit()` as today, with its cap of 16 GiB. The call
  is written once, with its arguments, in the helper of decision 2, and
  the first chunk's 64 kB beside it. The join asks once a build and
  keeps the answer, since its check before each batch needs it; a
  build after `work_mem` changed, at a cursor's next fetch, asks
  again. The join's
  values keep their own rule of size: a value larger than a quarter of
  1 MiB takes a chunk of its own size, and an ordinary chunk of values
  is the larger of the rule's length and the value.
- **Why:** the function exists, is in the spec of `hash-table`, is
  tested, and is what `spill-crate` moves into `relspill`. For every
  limit it gives what the join and the grouping compute today: both
  take an eighth, bound it by 64 kB and 1 MiB and round down to 8,
  and both bounds are multiples of 8. No new entry point; one call per
  chunk of 64 kB to 1 MiB.
- **Refused:** a C formula of the rule shared by the nodes (a second
  owner beside the level plan); a rule in `relhash` (the share of
  `hash_mem` is the engine's policy, and its contract keeps it free of
  the host's limits).

### 2. One internal helper for a directory and a chunk

- **Was:** each node grows its arrays of bases and lengths, allocates
  a chunk, initializes it through the kernels, checks the most chunks
  and raises 54000 with its name, in its own way; the merge of a
  grouping's reload skips the check.
- **Will be:** `nodes/table_chunks.c`, not a public header, with two
  operations:
  - **reserve** the directory for a number of chunks, in the
    directory's own memory context, raising 54000 with the node's
    message before the number passes `TESS_TABLE_MAX_CHUNKS`; it also
    serves a node that adopts chunks it did not allocate, a shared
    join's or chunks loaded from a file, and counts the grouping's two
    reserved slots;
  - **add** a chunk: allocate it in the chunks' memory context, which
    may differ from the directory's, initialize it, list it, and
    return its number.

  The caller keeps what is its own: the grouping's partition lists and
  their bytes, the reserved slots' contents, the join's counter of
  chunks, the table's view of the arrays. The shared join keeps its
  allocation in DSA and calls only reserve. The join's values do not
  use it: their references, their limit and their chunks differ.
- **Why:** the directory and the error are what every table repeats;
  one copy keeps them alike, and the merge gets its check by using it.
- **Refused:** an allocator over memory contexts and DSA (one DSA
  caller, with headers and a budget the others lack); a public header
  in `include/tessera/` (every caller is in `nodes/`).

### 3. One note of a table's growth

- **Was:** the grouping notes its peak after a chunk and samples both
  indexes in `regrow_table` but leaves out its DISTINCT sets and its
  spill; a DISTINCT set notes nothing, its first index included; a
  grouping's reload merges chunks without counting them, starts its
  merge from a stale total, ends it with the first index's size, and
  in `agg_table_from_parts` frees the old index before it notes.
- **Will be:** each node has one note of memory that takes the bytes
  of a replaced index still alive. Every path that adds a chunk, makes
  a first index or replaces one calls it, the replacement while both
  indexes are alive. The totals it reads are kept right at every step:
  a merge adds its chunks, starts from a reset total and ends with the
  index it has.
- **Why:** a peak that misses its highest moment is not the peak, and
  a total that is stale makes the next note wrong too.

### 4. The serial join checks its index before a batch

- **Was:** before a batch the serial join spills when its chunks, its
  values and room for two more chunks pass `hash_mem`; the index comes
  after the last batch and is counted then.
- **Will be:** the check before a batch also counts the exact size of
  the index the build will need for its records and the rows the batch
  selects, through the kernels' `table_size`. A row a filter rejected
  is not counted; a row with a NULL key, which makes no record, is,
  since counting those would read the keys before the check. That is a
  check, not memory taken: the node's bytes and its peak count the
  index when it is built, as today, and the plan of a spill's first
  level reads the bytes as today. The room for two more chunks stays.
- **And:** a spill reserved 8 bytes a row for the index of its
  resident partitions, whose buckets take 8 to 16 bytes a row: it now
  reserves that index's exact size, until the index is made. A shared
  join's participant ends the reservation when it hands its resident
  rows to the shared table, whose index the shared build reserves;
  before, it kept charging an index it did not have while it joined
  the partitions on disk.
- **Why:** the memory a built table takes is its chunks and its index;
  a check of the chunks alone lets the index pass the limit.
- **Cost:** the room for two chunks was already a margin: a build
  whose chunks came within 2 MiB of the limit spilled. With the index
  in the check, a build whose table and index fit but leave less than
  that room spills too: 300 000 records of 32 bytes take 14 745 696
  bytes with their index, and the check passes 16 MiB after 296 960
  records. Such a build pays a spill it did not need; the measurement
  gives that case. Budgeting the next batch's own allocations instead
  of the room for two chunks would spare it, and is left to task 3.23
  of `spill-crate`, the rule of what may pass the limit.
- **Refused:** the shared join's `record_chunk_cost`, which charges the
  index of every record a chunk could hold and a header of DSA the
  serial chunks lack: it spills earlier still, and counts as taken
  memory that is not.

### 5. A grouping's first index by its buckets

- **Was:** the first capacity is capped at `hash_mem / 32` groups.
- **Will be:** it is capped at the most groups whose buckets, 4 bytes
  each and rounded up to a power of two, fit a quarter of `hash_mem`;
  the index's header of 96 bytes is beside it; the least capacity the
  table takes stays the floor.
- **Why:** the quarter was meant for the buckets. At a limit that is a
  power of two the cap is the same as today; at 3 MiB it drops from
  98 304 groups to 65 536, and the buckets from 1 MiB to 512 kB.
- **Refused:** a cap by the index's bytes with its header, which halves
  the first index at every power-of-two limit for 96 bytes.

## What we do not do

The Non-Goals above, with their reasons. No change of the first chunk,
64 kB, nor of the most chunk, 1 MiB.

## Risks / Trade-offs

- [Expected outputs change beyond Memory Usage] → a serial join near
  the limit may now spill, and then its chunks, partitions, Bloom
  filter and resident rows change in EXPLAIN; a grouping's regrows may
  change. Each changed line is read, and the suites compare Tessera
  with the core for every result.
- [A join that now spills is slower] → the one of 420 000 rows went
  past `hash_mem` before; the one of 300 000 did not, and now spills
  for the margin of decision 4. The measurement gives both, and a join
  of 250 000 rows, which must not change.
- [Smaller DISTINCT chunks, more allocations] → at most one more
  allocation an eighth of `hash_mem`; measured.
- [The helper changes code shared by six paths] → its commit carries
  the tests of each path's lifecycle (build, spill, rescan, reload,
  shared attach) through the suites, and the disassembly of the module
  `nodes` is read.

## Tests

Each fault gets a test that fails on the code before its fix; the
tests of the helper and of the rule check that nothing changes, and
pass before as after. Memory Usage is the node's peak, in kB. Where the
node counts only its own chunk and index lengths (a table in memory,
integer keys and arguments, no dictionary), the peak is exact and a
test compares it with the bytes the sizes of `relhash` give. Where it
counts memory contexts (a spill), the allocator adds to it, and a test
gives a lower bound.

- The note of growth (first): one group, `count(DISTINCT v)` of 3000
  distinct integers at `work_mem` = 256 kB: the peak counts the set's
  chunks and its last regrow's two indexes, 1205 kB with the chunk of
  1 MiB still in place (before: 73 kB, the set's growth unseen); two
  sets, the second regrowing while the first is alive, its regrow the
  highest moment; a grouping that regrows while its sets are alive; two
  DISTINCT aggregates over no rows, their first indexes counted.
- DISTINCT chunks (after the note): the same query's peak drops to
  245 kB, the chunk of 1 MiB replaced by one of 64 kB.
- The reload: in assert builds, each note checks that the node's count
  of its table equals the chunks it owns, its two reserved slots
  (the source and the empty chunk) left out, and the index it has;
  the reload case of `agg` that merges and regrows fails that check
  before the fix. The rebuild of a spilled table notes both indexes as
  a regrow does, which the DISTINCT cases test; a case where that
  rebuild is the node's highest moment was not found, since the node
  evicts partitions first and frees more than the new index takes, so
  that note is shown by review.
- The serial join's index: 420 000 inner rows of one integer key and
  no payload at `hash_mem` = 16 MiB, serial, the plan checked to be
  TessHashJoin: it spills and its peak is within `hash_mem` (before:
  over, its chunks and index alone 17 891 424 bytes); 250 000 rows
  stay in memory before and after; the results against the core. At
  13 MiB, 262 150 rows spill in their last batch, where the index's
  buckets double, and a filter that leaves 262 144 of them stays in
  memory (before: it spilled, the check counting the rows it rejected).
  The shared participant's reservation is shown by review: which
  participant joins which partition on disk depends on timing.
- The first index: an estimate far above the cap over no rows at
  `hash_mem` of 2, 3 and 4 MiB, whose first index is the peak: 513,
  513 and 1025 kB (at 3 MiB before: 1025); an estimate below the cap
  and one below the least capacity at 3 MiB.
- The merge's check: reaching 32768 chunks in a merge takes hundreds
  of MiB of chunks at the least, and a fixture that fakes the count
  would let the code before the fix write past its directory. The
  scenario is "review only": the review lists every write of a chunk
  into a directory in `nodes/` and shows that each goes through the
  helper, whose directory is tested at 32767, 32768 and 32769 chunks,
  its reserved slots counted.
- The rule: `tess_spill_chunk_len` for one partition at a share of 8,
  at limits of 256 kB, 512 kB, 524 352 bytes, 3 MiB, one byte below
  8 MiB and 8 MiB: 65 536, 65 536, 65 544, 393 216, 1 048 568 and
  1 048 576 bytes; the callers checked in review to take it.

## Measurement plan

- **What:** the time and the memory of queries whose tables grow,
  against the commit that adds the new cases.
- **Cases:** of `join`, `fk_count`, `dup` and `left_dup` (tables that
  fit); of `spill`, its join and grouping cases, and six cases of its
  own whose tables the family makes, at `hash_mem` twice `work_mem`
  (the family sets the multiplier): serial joins built on 250 000,
  300 000 and 420 000 integer keys at 16 MiB (`build_*`); the fact
  table grouped by its key at 3 MiB, above the cap of the first index
  (`group_fk_1536kb`); a `count(DISTINCT v)` by 10 groups over 2 M rows,
  3000 pairs, at 256 kB, admitted by the planner's gate through
  extended statistics (`distinct_128kb`); and 300 000 groups at 256 kB
  whose partitions regrow their index while they merge
  (`regrow_128kb`). Each case's plan is read in the run's `plans.txt`
  to be Tessera's, and its Memory Usage is given.
- **Tool:** `cargo ab measure --base <that commit> --family join
  --family spill --family anyagg --cases <the cases> --repetitions 11`,
  release builds, on an idle machine the person confirms, with
  `PGOPTIONS='-c statement_timeout=30s'`, so that no statement, the
  warm-ups and the final EXPLAIN ANALYZE included, runs past 30
  seconds; `bench/pg/run.sh` now keeps the options it is given, beside
  its own. The cases are split into runs of five minutes at most.
- **When:** each commit that changes when a table grows or spills is
  measured on its cases against its parent (3.2 the DISTINCT case, 3.4
  the joins, 3.5 the grouping at 3 MiB), and the whole once at the end
  against the commit of the cases.
- **Threshold:** no case flagged by the tool, but the joins of 300 000
  and 420 000 rows, which now spill: their time and memory are given
  with their plans.
- **Machine code:** `tessera-bench --base HEAD~1 --disasm --module
  nodes` on the commits of the helper and of the length rule: no loop
  of a batch changes.
