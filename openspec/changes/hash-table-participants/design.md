## Context

The hash table (capability `hash-table`) is shared by the participants
of a parallel join. Their build goes through phases that PostgreSQL's
barrier separates; a participant is a state machine of the table's C
API (`tess_build_step`) that returns actions and never waits itself, so
that the waits, which may raise errors, stay in the C node. When the
table spills, each partition on disk is joined in a round of its own,
with its own phases (`tess_round_step`). A table that spills, shared or
a process's own, keeps its decisions in words (`tess_table_spill_*`):
the partitions in force, the bytes of each partition against a budget,
which went to disk, and the counters of the rounds. The text is in
`docs/table.md`, `docs/spill.md` and `tessera/table.h`; the code in
`phases.rs` and `shared_spill.rs` of the kernels; the loom model in
`loom.rs` checks the phases, the split, the eviction and the rounds.

## Goals / Non-Goals

**Goals:**
- One place that says what the shared build, its rounds and the words
  of a spill promise, with every statement tied to a test.
- The hang of a join that spills removed, and every value out of range
  refused.

**Non-Goals:**
- How the join uses the phases, when it spills and how many partitions
  it makes: the join's own capability.
- The plan of a spill's partitions (`tess_spill_partitions`,
  `tess_spill_chunk_len`): capability `spill-format`.
- The aggregate states in a payload (`tess_table_accumulate*`) and the
  items of a sort (`tessera/sort.h`): the capabilities of the grouping
  and the sort.

## Decisions

### A partition chosen again

**Was:** the rule returns a partition already on disk when, weighed by
`spilled`, it holds the most; a grouping writes the groups it holds of
it and frees them. A join demoted it as if it were in memory: it wrote
its empty tail, kept the chunk, freed nothing, and asked again. With a
weight above 0 and no limit a check, the loop never ended, and it read
no interrupt.

**Will be:** a join writes what it holds of a partition on disk, its
values and its tail, and frees them, as it does when its level starts
joining; when it holds nothing of it, the check ends. Both loops check
for interrupts.

### Outputs before writes

**Was:** `tess_build_totals` wrote the records, then checked the other
outputs.

**Will be:** every output is checked first.

### Values out of range

**Was:** an attach answering a phase past FREE gave a detach, bits past
32 wrapped in the split rule, counts wrapped past 2^64 - 1 and file
numbers past 2^32 - 1, and words were attached for any capacity.

**Will be:** each is refused, with SQLSTATE `XX000`, as every argument
of a call outside its range is.

### What we do not do

- A participant's state is zeroed by the node when the shared memory
  is initialized, which a gather does before it runs the plan again; a
  rescan in another order would need it zeroed by the step itself.
  Nothing calls it so, and the design says so.
- A participant that attaches when a build that spilled is over leaves
  at once and joins no round; the others join every partition, so the
  rows are right and only parallelism is lost.
- An error between two phases leaves the other participants until the
  parallel query ends, as the core's parallel hash join does.
- The loom model links a participant's files once, after it loads them
  all; the join links each as it loads it. The model shows the order of
  the barrier, which is the same.
