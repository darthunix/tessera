# table-memory: how it is built

A join and a grouping of Tessera keep their rows in a hash table of the
crate `relhash` (see the design of
[hash-table](../hash-table/design.md)). The table lives in memory
the node owns: an index of buckets, and chunks that hold the records.
The node allocates each chunk in a PostgreSQL memory context, or in
shared memory (DSA) for a parallel join, asks the kernels to prepare
it, and lists it in the table's directory: two arrays, of the chunks'
addresses and of their lengths. A join makes its index once, after its
rows are in. A grouping sizes its index for a number of records, its
capacity, and when half its buckets are taken rebuilds it larger: a
regrow.

When a table does not fit in the node's memory, the node spills: it
sorts the records into partitions by their hash, keeps some partitions
in memory, writes the others to disk and reads them back later. The
partitions of one pass are a level; a partition read back that is still
too large splits into a level below. The plan of a level says how many
partitions it has and how long their chunks are.

This page explains how a table grows while it stays in memory: how long
each chunk is, how the directory grows and where it stops, what the
node counts of its memory, and how a join in one process knows that its
index will fit. The rules seen from SQL are in [spec.md](spec.md) next
to this page. How a table that does not fit is split into partitions
and sent to disk is in the design of hash-table.

## The problem

Three kinds of table grow in memory: the table of a hash join
(`TessHashJoin`), the table of a grouping (`TessAgg`), and the set of
each DISTINCT aggregate of a grouping, which holds the pairs of group
and value it has seen. Each node grows its tables, and each must answer
the same questions.

1. How long is the next chunk? A long chunk passes a small limit by
   itself; a short one makes a large table take many allocations.
2. What happens at the most chunks a table can address, 32768?
3. What does the node count of its memory? It shows the most as
   Memory Usage in EXPLAIN ANALYZE, and compares its bytes with its
   limit, `hash_mem`, to decide when to spill. `hash_mem` is
   `work_mem` times `hash_mem_multiplier`, at most 16 GiB (see [The
   limit of memory](../hash-table/design.md#the-limit-of-memory)).
4. A join in one process makes its index only after its last row. How
   does it know before then that the index will fit?

When each node answers on its own, the answers drift apart: a rule
written in three places changes in one and stays as it was in the
others.

## Goals and what they cost

The numbers were measured on the developer's machine, an Apple M5 Pro.

- **One owner of each rule** (questions 1 and 2). The length of a chunk
  comes from one function, the plan of a spill's level, for every table
  in memory. The check of the most chunks, and the directory of most
  tables, are one helper of the nodes. The price is a call into the
  kernels for each chunk past the first, or once a build for a join.
  The runs flagged slower only joins that now spill, once each other
  flagged case was measured again alone: a first flag of the planning
  time did not repeat. A DISTINCT set under a small limit takes chunks
  of an eighth of `hash_mem` in place of chunks of 1 MiB: smaller
  allocations, and more of them.
- **A peak that misses no step of growth** (question 3). Every step
  that adds a chunk, makes an index or replaces one notes the node's
  memory, a replacement while both indexes are alive. The price is
  totals kept right at every step of a grouping's reload. One moment of
  a grouping's spill is still missed: see [What Memory Usage
  counts](#what-memory-usage-counts).
- **An index inside the limit** (question 4). A join in one process
  checks, before each batch, the size of the index its records will
  need. The price is a spill for a build that would just have fitted
  (see [When a join in one process
  spills](#when-a-join-in-one-process-spills)).

## The whole in one picture

```
  TessHashJoin          TessAgg            DISTINCT set
  serial, parallel,     the grouping,      one for each DISTINCT
  a spill's levels      its reload         aggregate, all groups
        |                    |                    |
        v                    v                    v
  +--------------------------------------------------------+
  | the nodes' helper of a table's chunks                  |
  |   check the most chunks: 54000 past 32768              |
  |   reserve a directory, grown by doubling               |
  |   allocate and prepare a chunk                         |
  |   the length of a chunk past the first                 |
  +--------------------------------------------------------+
                             |
                             v
  +--------------------------------------------------------+
  | the plan of a spill's level, in Rust                   |
  |   a table in memory is one partition at a share of 8   |
  +--------------------------------------------------------+
```

Each node keeps what is its own: where a chunk goes in its directory,
its counters, its partitions, its note of memory and its check of when
to spill.

## The length of a chunk

The first chunk of a table is 64 kB, so that a table of a few rows
takes little memory. Every later chunk takes an eighth of `hash_mem`,
no less than 64 kB and no more than 1 MiB, the most a chunk may be.

The rule is not written in C. The helper asks the plan of a spill's
level, in the crate `relspill`, for the chunks of one partition at a
share of 8 (see [How a level is
planned](../hash-table/design.md#how-a-level-is-planned)). A spill's
levels take their chunks' length from the same plan; a table in memory
is that plan with one partition. Each process of a parallel join takes
its own first chunk of 64 kB.

A join asks once for each build and keeps the answer, since its check
before every batch needs it. A build after `work_mem` changed, as at
the next fetch of a cursor whose join is scanned again, asks again.

The values of a join, such as text longer than a word, keep a rule of
their own: a value larger than a quarter of 1 MiB takes a chunk of its
own size, and an ordinary chunk of values is the larger of the rule's
length and the value.

Refused:

- A C formula of the rule shared by the nodes: a second owner beside
  the level plan, which the spill already uses.
- The rule inside `relhash`: the share of `hash_mem` is the engine's
  choice, and the table stays free of the host's limits.

## The directory and its limit

A reference to a record has 32 bits, so a table addresses at most 32768
chunks (see [The limit of
memory](../hash-table/design.md#the-limit-of-memory)). The nodes'
helper does three things:

- it checks a number of chunks, and fails with SQLSTATE 54000 and the
  node's message when a table would hold more than 32768;
- it reserves a directory for a number of chunks, after that check: two
  arrays grown by doubling, in the directory's own memory context;
- it allocates a chunk in a memory context, which may differ from the
  directory's, and prepares it through the kernels.

The caller lists the chunk in its directory and keeps its number. The
serial join, the grouping, its reload and the DISTINCT sets reserve
their directories through the helper; they and the grouping's spill
allocate their chunks with it. A process of a parallel join reserves
its own view of the shared table's directory through the helper too,
when it attaches to the table and when it loads a round read back. The
rest keep arrays of their own and call only the check before they grow
one: a spill's sides, which also load chunks from a file and borrow one
to split; and a parallel join's chunks in DSA, when a process takes the
number of a new chunk and when the shared table publishes its
directory. A split of a
grouping's partition writes a directory of fixed size, which its chunks
cannot pass. A grouping's spill keeps two slots of its directory, for
the chunk it reads and an empty one, and counts them. The join's values
keep a directory of their own, with their own references and limit.

The limit of memory keeps a node that spills far from 32768 chunks, so
there the check is the last guard. A DISTINCT set never spills: the
planner takes it only when its estimate fits `hash_mem`, and past that
estimate the check is its guard.

Refused:

- An allocator over both memory contexts and DSA: one caller uses DSA,
  with headers and a budget the others lack.
- A public header: every caller is a node of Tessera.

## What Memory Usage counts

Each node has one note of its memory. It adds up the node's bytes, the
chunks and indexes of its tables and an index being replaced while it
is still alive, and keeps the most. Every step that adds a chunk, makes
a first index or replaces one calls it; a regrow calls it while both
indexes are alive. The set of a DISTINCT aggregate counts from its
first index on.

The totals the note reads are kept right at every step. When a grouping
merges a partition read back, the merge counts the chunks it adds,
starts from a reset total and ends with the index it has. In a build
with assertions each note of a grouping checks that its count of its
table equals the chunks it owns, the two reserved slots left out, and
the index it has.

One moment is still missed. When a grouping starts to spill, it copies
each chunk of its table into the chunks of its partitions and frees the
original without a note; a split of a partition read back resets its
source and writes out the chunks of the level below before it notes.
While the copies and the originals are both alive, the node holds more
than its note shows.

In a parallel plan, Memory Usage is the sum of each process's most,
which need not have come at the same time.

## When a join in one process spills

A join that builds alone appends all its rows first, then makes the
index for exactly the records it got. Before each batch it checks
whether to start to spill. It adds its chunks and values, room for two
more chunks, and the exact size of the index its records and the rows
the batch selects will need, which the kernels give. A row that a
filter rejected is not counted. A row with a NULL key is counted,
though only a RIGHT or FULL join keeps it as a record: telling it apart
would read the keys before the check. The check is not memory taken:
the node counts the index when it makes it.

When a spill starts, it reserves the exact size of the index of the
partitions that stay in memory, until it makes that index. A process of
a parallel join ends the reservation when it hands those rows to the
shared table, whose index the shared build reserves.

The room for two chunks was a margin before the index came into the
check: a build whose chunks came within 2 MiB of the limit spilled.
Now a build whose table and index fit, but leave less than that room,
spills too. 300 000 records of 32 bytes take 14 745 696 bytes with
their index, and the check passes 16 MiB after 296 960 records. Such a
build pays a spill it did not need. By the median of the runs, a join
of a fact table of 2 M rows against 300 000 keys took 1.64 times as
long as in memory, and one against 500 000 rows with a text column at
32 MiB, whose table took 31 881 kB in memory, 1.33 times. Counting
only what the next batch can allocate, in place of room for two
chunks, would spare such a build; that is a rule of what may pass the
limit, not of how a table grows.

Refused:

- The parallel join's charge for a chunk, which counts the index of
  every record the chunk could hold and a header of DSA that a serial
  chunk lacks: it spills earlier still, and counts as taken memory that
  is not.

## A grouping's first index

A grouping makes its first index for the planner's estimate of its
groups, with the least capacity of the table as the floor. The estimate
is capped at the most groups whose buckets, 4 bytes each and rounded up
to a power of two, fit a quarter of `hash_mem`; the index's header of
96 bytes is beside them. At a limit that is a power of two that is
`hash_mem / 32` groups; at 3 MiB it is 65 536 groups and 512 kB of
buckets.

Refused:

- A cap by the index's bytes with its header, which halves the first
  index at every limit that is a power of two, for 96 bytes.

## What we decided not to do

- A spill for a grouping with a DISTINCT aggregate. It never spills;
  the planner takes it only when its estimate of the pairs of group
  and value fits `hash_mem`.
- The key dictionary of a grouping, PostgreSQL's simplehash: another
  table, with another allocator.
- The chunks of `TessRows`, whose length comes from their caller, a
  sort under `work_mem`.
- The limit of a level below a spill, and the count of a spill's block
  lists, marks and buffers: rules of the spill, not of a table in
  memory.
- The first chunk and the most chunk stay 64 kB and 1 MiB.

## Files

- `nodes/table_chunks.h`, `nodes/table_chunks.c`: the check of the most
  chunks, a directory, a new chunk, and the length of a chunk past the
  first
- `nodes/hashjoin.c`: the serial build, its check before a batch, its
  kept length of a chunk and its note of memory
- `nodes/hashjoin_shared.c`: a parallel build's chunks in DSA, and the
  end of a process's reservation of its index
- `nodes/hashjoin_spill.c`: the directories of a spill's sides, and the
  reservation of the index of the partitions in memory
- `nodes/agg_group.c`: the grouping's chunks, its first index, its
  regrow and its note of memory
- `nodes/agg_keydict.c`: the sets of the DISTINCT aggregates
- `nodes/agg_spill.c`: a grouping's reload, its merge and its totals
- `crates/relspill/src/plan.rs`: the plan of a level, which gives the
  length of a chunk

## Tests

- `test/sql/agg.sql`: the Memory Usage of a DISTINCT set under a small
  limit, of a regrow while another set is alive, and of DISTINCT over no
  rows, each against the bytes the sizes of `relhash` give; a reload
  that merges and regrows, whose every note checks its totals in a
  build with assertions; the first index at limits of 2, 3 and 4 MiB.
- `test/sql/join.sql`: serial builds of 250 000 and 420 000 rows at
  16 MiB and of 262 150 rows at 13 MiB, whether each spills and its
  rows against the core's; the last with a filter, whether it stays in
  memory; a table built again in one query after `work_mem` fell.
- `test/sql/table.sql` with `test/tessera_table_test.c`: the directory
  reserved for 32767, 32768 and 32769 chunks.
- The unit test `a_table_in_memory_takes_an_eighth_of_the_limit` in
  `crates/relspill/src/plan.rs`: the length of a chunk at six limits.
- By review only: a merge at the most chunks, since 32768 chunks take
  hundreds of MiB; the note of both indexes when a spilled grouping
  rebuilds its table, which no case found makes the node's highest
  moment; and the end of a parallel process's reservation, since which
  process joins which partition depends on timing.
