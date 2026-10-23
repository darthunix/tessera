# hash-table: how it is built

A hash join and a grouping both need to find rows by a key fast.
Tessera keeps such rows in the hash table of the crate `relhash`, and
its nodes in C call it through `tessera/table.h`. The table itself,
what it is made of, how a batch of rows goes in and how it is looked
up, how it grows, and how several processes use one table at once, is
explained in the design of [relhash](../relhash/design.md), and its
rules are in the spec of [relhash](../relhash/spec.md). That design
also explains the words used below: index, chunk, record, reference,
append, link, probe, one writer and participant, and what the crate
gives a caller to keep beside the table: partitions and their levels,
a Bloom filter of the keys, and the marks of a RIGHT or FULL join.

This page explains what Tessera builds around the table. First how the
nodes use it: how they size it, how the keys of each SQL type and the
values longer than a word go in, the limit of memory, and what a call
checks. Then how a join and a grouping use partitions when the table
spills to disk, a Bloom filter of its keys, and the marks of a RIGHT or
FULL join. Last, it explains how the processes of a parallel join build
one table together, step by step, and agree on which of its partitions
go to disk. The exact bytes,
limits and errors are written once, in the spec, [spec.md](spec.md)
next to this page. Where this page leans on a rule of the spec, it
links to the requirement that states it. What a join or a grouping
does with the table is described with those nodes.

## The problem

The table answers the questions every table of a database engine
meets, as the design of `relhash` shows: among them how it sorts its
records into partitions, how a row without a pair is dropped cheaply,
and where a join keeps the records that found a pair. Tessera's nodes
add two of their own.

1. A table may not fit in the memory a node is given. The table sorts
   its records into partitions, but a node must choose how many a level
   gets, which go to disk, and what to do with a part read back that is
   still too large. How does each node choose, without a rule of its
   own?
2. In a parallel join every process builds a share of one table, and
   the table may spill. Between the steps of the build all of them
   must wait for each other, and a wait in PostgreSQL may raise an
   error, which must not pass through Rust code. How do the processes
   keep in step, and agree on what goes to disk, when no one of them
   leads?

## Goals and what they cost

Each goal answers one of the questions above. Each has a price, and the
price is written next to it. The numbers were measured on the
developer's machine, an Apple M5 Pro; the target platform is Linux on
x86-64, where they have not been measured yet.

- **One rule for every node** (question 1). A node plans a level before
  it fills it, by one rule with parameters of its own, and sends
  partitions to disk by one rule with weights. A partition read back
  that is still too large splits into a level below, or a join takes it
  in pieces. The price is a copy at each split, and parameters that
  each node must set.
- **Steps that never wait** (question 2). A participant of a shared
  build is a small state machine of the table's C API. Each call
  returns the next action, such as build, wait, link or probe; the node
  does it, waits included, and passes what the wait returned to the
  next call. What the participants decide together, the split, the
  partitions on disk and the files each one reads, lies in words every
  one of them maps, changed by atomic operations, and the first one to
  change a word decides for all. The price is a call for each step,
  and a loop in the node that must follow the actions in their order.

## How the nodes size a table

The nodes size the index in different ways. A join in one process
appends its whole inner side first and then makes the index for exactly
the rows it got. A grouping makes its index for the planner's estimate
of the groups, no larger than its memory allows, and moves to a larger
one when its records reach half the buckets (see [One
writer](../relhash/design.md#one-writer)). A parallel join makes the
index once, after all processes have appended (see [Putting rows
in](../relhash/design.md#putting-rows-in-append-then-link)).

The nodes make the first chunk of a table small, 64 kB, so that a
table of a few rows takes little memory. Later chunks are larger: an
eighth of the node's memory limit, but at most 1 MiB, the limit of a
chunk. A node may also pass a chunk of 8 bytes, its used mark alone,
where it has no chunk to give for a number: nothing fits in it, so
nothing is ever written there.

## Keys of each SQL type

The table compares keys as words, bit for bit, because that is the
cheapest comparison there is: one instruction a key, and no call of a
type's functions. So a node puts each column of a key into its slot in
one of two forms. Which type takes which form is listed in
[The SQL types of a key](spec.md#requirement-the-sql-types-of-a-key).

- **The value itself.** PostgreSQL keeps a whole number, a date, a
  boolean and a timestamp as an integer of at most 8 bytes, and two of
  them are equal exactly when their integers are. The node puts that
  integer into the slot.
- **A number that stands for the value.** For other types, equal values
  may differ in their bytes: numeric 1.0 and 1.00, float8 -0 and 0, or
  'a' and 'A' under a case-insensitive collation. A string does not fit
  in a slot at all. So the node makes a number of 64 bits from the
  value with the type's own functions, which know when two values are
  equal, and puts the number into the slot.

A grouping and a join make that number in different ways.

- **A grouping** keeps a dictionary of the values it has met: a hash
  table of the node's own, in C, that hashes and compares the values
  with the type's functions. The first of each set of equal values
  gets the next number, and the table groups the rows by the numbers.
  Equal values get one number, so they make one group, and different
  values never share a number, so a comparison of the numbers is
  exact. When a group goes out, the dictionary gives its key back as
  the value.
- **A join** puts in the value's hash of 64 bits, made by the type's
  own hash function. A hash needs no dictionary shared by the two sides
  of the join, which are read at different times and, in a parallel
  plan, by different processes: each row's number comes from its own
  value. The price is that two different values may have the same hash.
  So the join keeps the type's equality as a join filter, which EXPLAIN
  shows, and checks it for every pair the table finds; the inner row's
  value lies in the payload for it. The join's cost model, measured on
  the developer's machine, counts 18.6 ns a row for a hashed key, where
  a probe by a word costs 1.9 ns a row.

A type without a hash function, such as `money`, cannot take the
second form, so a grouping or a join by it stays the core's. Nor does
a join take the equality of two different types other than the
integers, such as `float4` with `float8`: both sides must make their
numbers with one function. Some types whose values would fit in a
word, such as `time`, still take the second form: only the types the
spec lists go in as their value.

## Values longer than a word

All records of a table have one size, so a record cannot hold a value
whose length varies, such as a string. A key never needs to: a key of
such a type goes in as a number of 64 bits (see "Keys of each SQL
type"). A payload keeps such a value outside the table. The node copies the
value's bytes into blocks of its own, its **chunks of values**, and the
payload's word for that column holds a reference to the copy: the
number of the chunk of values plus one, and the byte where the copy
starts. To the table this word is 8 bytes like any other. The spill
format writes the same words to disk and defines them, in [A reference
to a by-reference
value](../spill-format/spec.md#requirement-a-reference-to-a-by-reference-value).

```
 a record of a join on id that keeps the columns amount and name
 ┌────────┬────────┬───────────┬────────┬──────────────────┐
 │ header │ key    │ NULL bits │ amount │ name             │
 │        │ id: 42 │ of amount │ 1500   │ chunk of values  │
 │        │        │ and name  │        │ 0, byte 64       │
 └────────┴────────┴───────────┴────────┴────────┬─────────┘
                                                 │
 chunk of values 0, the node's own memory        ▼ byte 64
 ┌───────────────────────────────────────────┬─────────┬───────┐
 │ other values                              │ 'Smith' │ free  │
 └───────────────────────────────────────────┴─────────┴───────┘
```

## The limit of memory

A reference of 32 bits sets the limit of a table: at most 32768
chunks of 1 MiB, 32 GiB of records (see
[References](../relhash/design.md#references)). A node keeps its tables
far from that limit. A join or a grouping sends rows to disk when its
memory reaches `hash_mem`, and a sort when it reaches `work_mem`; none
of them counts its chunks. Each takes its limit through
`tess_table_memory_limit`, which keeps it at most 16 GiB, half of what
the chunks can hold, so that the node sends rows to disk
long before its table would need a 32769th chunk, every participant's
short first chunk included. A parallel join keeps the chunks of every
participant in one table, so the limit bounds their sum, not each
share. The price falls only on a server whose `hash_mem` or `work_mem`
is above 16 GiB: there a node sends to disk a table it could have kept
in memory. Without the limit it stopped the query with an error of
SQLSTATE `54000`, "program limit exceeded", where PostgreSQL's own node
went on.

## Checks and errors

The table trusts nothing it reads, because it cannot know who wrote the
bytes last. Every call does four kinds of check.

- **The header, once a call.** The header's fields must agree with
  each other and with the length of the index, as
  [The index of a
  table](../relhash/spec.md#requirement-the-index-of-a-table) lists
  them. The calls that append to chunks before an index exists
  have no header to check. What makes a table damaged is in [A damaged
  table](../relhash/spec.md#requirement-a-damaged-table).
- **A chunk, when the call starts on it.** A call that writes or walks
  a chunk first checks that it is aligned and of a length a chunk may
  have; a chunk longer than 1 MiB would hold places that no reference
  can name. The other chunks a call is given it does not read, and
  their alignment is the caller's promise, as the validity of their
  memory is. Checking every chunk at every call cost 13 instructions a
  chunk, about 13 000 a call for a table of 1 GiB, whatever the rows. A
  debug build of the entry points still does it, so that the test
  suites find a node's wrong chunk wherever it lies.
- **Every reference before it is followed.** Its chunk must exist, the
  whole record must lie inside the chunk, and the record must claim the
  table's record size. The check uses the chunk's length, not its used
  mark. References come from calls over the same table, and in a
  shared table another process may be moving the used mark of its own
  chunk; reading it would race. A reference past the mark reads unused
  bytes of the chunk, never memory outside it.
- **Every walk is bounded.** A chain takes at most as many steps as the
  header counts records, and never more than the places a reference can
  name in the chunks given to the call. A chain that is longer has a
  loop in it. The second bound matters when the count itself is
  damaged: it comes from the number of chunks in the caller's own list,
  not from the table's bytes. It is loose, 2^17 places a chunk where a
  chunk holds fewer records, so that a damaged table with a loop is
  found later than it could be; in exchange it costs a shift, where a
  sum of the chunks' lengths cost every call two instructions for each
  chunk.

A failed check makes the call return an error status. It never crashes
the server and never loops. The calls check their arguments, the header
and the chunks before they change anything. A damaged record met in the
middle of a call may come after some rows were done, and a table that
gave such an error is not to be used again: the node raises the error,
and the query ends.

These errors mean a bug in Tessera or in its caller, not bad data from
a user or from a disk, so they are internal errors; the status and its
SQLSTATE are in
[The status of an error](spec.md#requirement-the-status-of-an-error).
The C node
raises the error with `ereport` after the call returns, because a
PostgreSQL error must not unwind through Rust code.


## Partitions: a table that spills

A node is given a limit of memory. When its table would pass the limit,
the node keeps the records in **partitions** and writes whole chunks of
some partitions to disk, to read them back once its input is done. How
the table sorts its records into partitions, by the low bits of the
hash and a level at a time, is in [Partitions: a table that
spills](../relhash/design.md#partitions-a-table-that-spills) in the
design of `relhash`. How a chunk is written to disk is in
[spill-format](../spill-format/design.md). This section is about how a
join and a grouping use partitions, and how many partitions a level
gets.

A join and a grouping use partitions in different ways.

### A join

A join builds its table as usual, one index over its chunks, until the
table passes the limit. Then `tess_table_split` moves the table into
partitions one chunk at a time: the records of a chunk are copied into
the chunks of their partitions, and the chunk is freed before the next
one is split. So the records of only one chunk are in memory twice at a
time, never the whole table. Two things stay until the move ends: the
old index, which no lookup uses any more, and the values that are not
kept in the records, such as text: they are copied with their records,
and the old copies are freed together at the end. Then the node sends
some partitions to disk and keeps the others in memory; which ones is
the node's rule.

From then on the rows of the build side go straight into the chunks of
their partitions, by `tess_table_append_partitioned_columns`, and a
partition on disk writes each chunk as it fills. The partitions are not
linked while they fill, since no lookup needs them yet.

The lookup then comes in two steps.

1. When the build side ends, the partitions still in memory get one
   index over all their chunks, as one table. A row of the probe side
   whose partition is in memory looks it up at once. A row whose
   partition is on disk cannot be answered yet: it is written to that
   partition's file of probe rows, unless the Bloom filter of every
   build row shows that it has no pair anywhere.
2. When the probe side ends, the partitions on disk are taken one by
   one. A partition's records are read back and get an index of their
   own, and its probe rows are read back and look them up.

### A grouping, and the merge

A grouping must find the group of each row as the row comes, so its
lookup cannot wait. It keeps one index over the chunks of every
partition in memory. `tess_table_find_or_insert_partitioned` looks each
row's keys up in that index, as an ordinary find or insert does, and a
group that is not there gets a new record in the chunk of its row's
partition.

When the table passes the limit, the largest partition goes to disk
whole: its records are written and freed, and the index is made again
over the records left. The rows of that partition that come later find
no record in the index, since its groups are on disk, and make new
ones. So one group may end with records in several places: some on
disk, written at different times, and one in memory.

The **merge** makes one record of them again. When the input is done,
the grouping takes its partitions one by one. A partition's records in
memory get an index of their own, as a table. Its records on disk are
read back a chunk at a time and merged into that table by
`tess_table_combine`. A group the table has takes in the states read
back: a count adds to its count, a sum to its sum, a maximum keeps the
larger of the two. A group the table lacks is copied into a chunk the
caller names. Then every group of the partition is in memory once, and
the grouping returns them.

To merge, the grouping must know which states hold a value. A sum of no
rows is NULL, not 0, and so is a minimum. So a grouping that spills
keeps a word of flags at the start of its payload, a bit for each
aggregate, then a word for each aggregate. A count always has a value,
and its flag is not used. An example with a count, a sum and a maximum,
whose flags are bits 1 and 2:

```
              table's record   read back        after the merge
 flags        0b100            0b110            0b110
 count        3                2                5
 sum          (none)           7                7
 maximum      9                4                9
```

The rules of each kind of state are in [Merging groups read
back](spec.md#requirement-merging-groups-read-back).
A count or a sum that passes the int8 range fails with the error a
row-by-row grouping gives. The merge stops when the chunk it copies
into is full, or when the records reach half the buckets; the node
gives it a chunk or a larger index, and calls again from where it
stopped. While the index grows, the node hides the chunk it merges
from, since a regrow links every record of every chunk it is given.

### How a level is planned

Before a node fills a level, it plans it: how many partitions the level
gets, and how long their chunks are. One rule does it for every node,
`tess_spill_partitions` and `tess_spill_chunk_len`, and each node gives
it its own parameters. The exact rule is in [The partitions of a
level](spec.md#requirement-the-partitions-of-a-level) and [The chunks
of a level](spec.md#requirement-the-chunks-of-a-level).

The number of partitions starts at the node's least, 4, and doubles
while four things hold:

- It is below the node's most, 1024.
- The partitions would hold fewer bytes than the level expects, at half
  the limit each. A partition read back gets an index beside its
  records, so half the limit is about what one partition may take to
  be joined or merged in one piece.
- Twice as many partitions keep their **reserve** within half the
  limit. The reserve is what each partition keeps in memory while it
  fills: the chunk it appends to and the buffer of its file, and in a
  join the tails of the outer side too. With many partitions and little
  memory the reserves alone would fill it, so a small limit keeps the
  level at its least.
- The bits of twice as many partitions still fit in the hash. A level
  takes the bits after those the levels above took, its **shift**. A
  level past which not even the least partitions fit is refused; a
  node never asks for one, since a partition splits into a level below
  only while two bits are left.

A shared table also asks for two partitions for each participant at the
least, so that the participants spread over the rounds.

For example, a join of one process with a limit of 4 MiB keeps a
reserve of 48 KiB for each partition: four chunks of 8 KiB and two
pages. A level that expects 10 MiB starts at 4 partitions, which would
hold 8 MiB at 2 MiB each: fewer than 10, so it doubles to 8. Eight hold
16 MiB, and the doubling stops; their reserves, 768 KiB for twice as
many, fit in 2 MiB. At a limit of 256 KiB the same level keeps 4
partitions, since the reserves of 8, 384 KiB, pass half of the limit.

The chunks of a level share a part of the limit: an eighth in a
grouping and a sixteenth in a join, divided among the partitions. A
chunk is 1 MiB at the most, the largest a reference can name, and 8 KiB
or four records at the least, whichever is larger. In the example above
the chunks are 4 MiB / 16 / 8 = 32 KiB.

The grouping, the join and the shared table each had a rule of their
own in C. They became one rule with parameters, and a test compares it
with the three on many limits, shifts and sizes.

### A partition too large for memory

The node does not read a partition back to learn whether it fits: it
decides from what it counted while it wrote the partition, a join from
the bytes written, a grouping from an estimate of the groups. When a
partition is too large for the memory left, it is read back a chunk at
a time and split into a level below by the next bits of the hash, with
`tess_table_split` at a larger shift. Each chunk read back is split and
freed before the next one is read, and the new level's partitions go to
disk as memory runs short, as the first level's did. The new level is
then used as the first one was: a join joins its partitions with the
probe rows of the partition it split, and a grouping merges its
partitions one by one.

A join does not split a partition that holds nearly all of its level's
rows, nine in ten by default: those are the rows of one key, which every
level would put in one partition. It cannot split one when fewer than
two bits are left either. It joins such a partition in **pieces**: it
reads back as many of the partition's chunks as fit, gives them an
index, and reads all the partition's probe rows through it; then it
frees that piece, reads the next one, and reads the probe rows again. A
join other than INNER keeps a bit for each probe row that found a pair
in some piece: a SEMI join then returns a row once, and a LEFT or ANTI
join returns the rows without a pair in a last pass.

A grouping needs no pieces. A group is one record however many rows it
had, so a partition is as large as its groups, and its groups decide
whether it splits.

## The Bloom filter of the keys

A join may check the rows of its probe side against a Bloom filter of
the table's keys before it probes, so that a row without a pair costs
the read of a word, not a wait for its bucket. How the filter is built
and checked is in [The Bloom filter of the
keys](../relhash/design.md#the-bloom-filter-of-the-keys) in the design
of `relhash`. When it wants a filter, and whether it is worth its cost,
the join decides by its own rows. It uses one in three ways.

- **A table of one process.** After the build, `tess_table_bloom` fills
  a filter from every record of the chunks, and `tess_bloom_probe`
  checks each batch before the probe.
- **A shared table.** The participant that makes the index allocates
  the filter's words with the table, 16 to 32 bits a record of the
  query's shared memory, but only when a participant may want them: at
  the default rule, for a table of 4096 records at least, since a
  smaller one stays in the cache, where a miss costs less than the
  check. Each participant decides by its own rows whether it wants the
  filter, and the first that does builds it with
  `tess_table_try_build_bloom`; the others go on without it until it is
  ready.
- **A table that spills.** A filter of every row of the build side is
  filled as the rows come, before some of them go to disk: by
  `tess_bloom_add` in one process, or by every participant at once by
  `tess_bloom_add_atomic`. Rows of the probe side that it rejects have
  no pair in any partition, and are dropped before they are written to
  disk. A barrier orders every addition before the first check. This
  filter is sized within an eighth of the node's memory.

## The marks of a RIGHT or FULL join

A RIGHT or FULL join marks each record of its table that a pair
matched, with `tess_table_mark`, and after the probe side walks the
records without a mark, with `tess_table_next_unmarked`. [The marks of
a RIGHT or FULL
join](../relhash/design.md#the-marks-of-a-right-or-full-join) in the
design of `relhash` explains the marks. The C calls take the size of a
record, and check it against the index when the table has one. In a
shared table the marks lie in shared memory, a run sized for the
largest chunk for every chunk, changed by an atomic OR. The last
participant to leave the table walks the records without a mark. The
barrier it leaves through orders the marks of every other participant
before its walk.

A participant may leave before its share of the outer side is done:
under the core's `Gather` with a `LIMIT`, a worker stops once it has
returned the rows the limit asks for. The pairs of the rows it never
probed are never marked, and the last participant would return their
inner records as rows without a pair. So such a participant marks a
word the table's participants share before it leaves, and the last one
returns no record without a mark when the word is marked, as
PostgreSQL's parallel hash join skips its unmatched rows then. No row
is lost that the plan wanted: a worker stops so only after it has given
its consumer every row the limit asks for. The word is set before the
barrier and read after it, so the barrier orders it, as it orders the
marks; the loom model checks this, and that a mark set only after the
participant left can go unseen.

## A shared build

In a parallel join every participant reads a share of the inner side:
a parallel scan hands out its pages one at a time, and each participant
reads the pages it gets. All of them build one table from their shares,
and then all of them probe it with their shares of the outer side.

They must keep in step. No participant may link before the index
exists, and none may probe before every record is linked. PostgreSQL
gives a parallel plan a **barrier** for this, its `Barrier`. A barrier
counts the processes attached to it and has a **phase**, a number. When
every attached process has arrived, the phase goes up by one and they
all go on. One of them is **elected**: the barrier tells it that it
was the one to end the wait, so that it can do work that only one may
do. A process may attach late, and then it learns the phase the others
are in.

A build has seven phases, numbered as its barrier counts them. The
[spec](spec.md#requirement-the-phases-of-a-shared-build) names them
the same way:

```
 phase     every participant                  the elected one
 ───────   ────────────────────────────────   ──────────────────────
 0 BUILD   appends its share of the inner
           side to chunks of its own
           ═══════════ all wait ═══════════
 1 FLUSH   when the table spilled, writes
           its chunks of the partitions on
           disk
           ═══════════ all wait ═══════════
 2 SIZE    waits                              makes the index for
                                              exactly the records
                                              appended
           ═══════════ all wait ═══════════
 3 LINK    links its own chunks
           ═══════════ all wait ═══════════
 4 OUTER   when the table spilled, writes
           its share of the outer side to
           the partitions' files
           ═══════════ all wait ═══════════
 5 PROBE   probes, then leaves without
           waiting
 6 FREE    the last to leave frees the table
```

No phase copies a record, and the table never grows: the index is
made once, at SIZE, for the records the participants counted. "Growing
a shared table while it is built", under "What we decided not to do",
says why.

### A participant is a state machine

The order of the phases is the subtle part of a shared build, and it is
the part a model checker should check; the model checker of this page,
loom, checks Rust code. But the waits must stay in C. A wait may raise
a PostgreSQL error, and the error jumps out of the function with
`longjmp`. A jump over the frames of Rust functions is undefined
behavior in Rust.

So the table's C API holds the order, and the node does the waits. A
participant is a small state machine, `TessBuildParticipant`, that
never waits itself. Each call of `tess_build_step` takes what the last
action returned, the **reply**, and gives the next action; the node
does the action and calls again:

```c
TessBuildParticipant participant = {0};
uint32  reply = 0;

for (;;)
{
    uint32  action;

    tess_build_step(&participant, counters, reply, &action, &status);
    reply = 0;
    switch (action)
    {
        case TESS_BUILD_ATTACH:
            reply = BarrierAttach(&build);              /* the phase */
            break;
        case TESS_BUILD_ARRIVE_AND_WAIT:
            reply = BarrierArriveAndWait(&build, 0);    /* elected? */
            break;
        case TESS_BUILD_DO_BUILD:
            /* append this participant's share; report it */
            break;
        /* ... FLUSH, SIZE, LINK and OUTER the same way ... */
        case TESS_BUILD_DO_PROBE:
            return;     /* probe; leave through the same steps */
    }
}
```

The participant is three words of 4 bytes that the node keeps: the
phase it is in, where it stands between two steps, and whether it was
elected. They are zeroed before the first step: the leader zeroes its
own when it sets up the query's shared memory, which a `Gather` does
again before it runs its plan again, and a worker starts with a node of
its own.

One machine serves every participant, the leader and the workers, early
or late. A participant that attaches late joins the phase the others
are in, and does what is left of it:

- at BUILD it appends what is left of the inner side, which the scan
  still hands out; that may be nothing;
- at FLUSH and at OUTER it writes its share, which is what is left;
- at SIZE it waits, since it was not elected;
- at LINK it links its own chunks, of which it has none;
- at PROBE it probes with what is left of the outer side;
- at FREE the table is gone, and it leaves at once.

An example with two participants, A from the start and B late:

```
 phase   A                                B
 BUILD   attach: 0; append; wait
 FLUSH   write; wait
 SIZE    elected: make the index; wait
 LINK    link; wait                       attach: 3; link; wait
 OUTER   write; wait                      write; wait
 PROBE   probe; leave: not the last       probe; leave: the last
 FREE                                     free the table
```

A state that no step makes, or a phase past FREE after an attach,
cannot come from a sound node, and the step refuses it. An error in
any participant ends the parallel query, as in every parallel plan of
PostgreSQL; the other participants are stopped with it, and no step
needs to know.

### The counters of a build

Before SIZE the elected one must know how many records to make the
index for. Before that, every chunk needs a number unique over all the
participants, since a reference names a chunk by its number. Four words
that every participant maps keep this. Their layout is in [The counters
of a shared build](spec.md#requirement-the-counters-of-a-shared-build):

- A new chunk takes its number by an atomic addition of 1 to the third
  word, `tess_build_take_chunk`: the number is the value before the
  addition, so the numbers go 0, 1, 2, and so on.
- At the end of its share, a participant adds its records to the first
  word and sets its bits of the payload words that hold a NULL in the
  second, `tess_build_report`.
- After LINK, it adds the duplicates its links found to the fourth,
  `tess_build_add_duplicates`.
- `tess_build_totals` reads the four after a barrier.

For example, A takes chunks 0 and 2 and B chunk 1. A reports 900
records and the bits `0b01`, B 500 records and `0b10`. The totals are
1400 records, `0b11` and 3 chunks.

The bits are an OR, not a sum: a payload word holds a NULL in the
table's rows when it holds one in any participant's. The join reads a
column's NULL flags only when its bit is set, so a bit lost would turn
a NULL into a value, while a bit set in vain costs only a read.

Every operation on the counters is relaxed. An addition is a
compare-and-swap of the word from the count it read to the sum, tried
again when another participant changed the word between; a chunk
number is unique by the swap alone. The totals need no order of their
own, because they are read only after a barrier that every report came
before: a barrier counts its arrivals under a spinlock, and the
spinlock orders all that a process did before it arrived before all
that another does after the wait. An addition that would pass 2^64 - 1
is refused, and the counter keeps its value. A wrapped count of records
would make an index too small, and a wrapped chunk number would name
another participant's chunk.

The counters are written once over their words, `Counters<W>` of
`phases.rs`, as the words of a spill are: the words of shared memory
are one form, and the loom model's atomics another, so that the model
runs the real counters.

### Why FLUSH and OUTER

When the table spills, which "The words of a spill" below explains,
each participant holds chunks of the partitions that went to disk, and
only it may write them: a chunk has one writer. FLUSH gives every
participant the time to write its chunks of them and to finish its
files, before SIZE counts what stays in memory.

OUTER comes before PROBE because of a rule of PostgreSQL: a participant
that has started to return rows must not wait at a barrier. A worker
hands its rows to the leader through a queue, and when the queue is
full the worker waits for the leader to read it. If the leader waited
at a barrier for that worker at the same time, neither would ever go
on. So in OUTER, while no participant returns rows, each one writes its
share of the outer side to the files of the partitions on disk. In
PROBE, and in the rounds after it, a participant only reads files, and
it leaves a barrier without waiting.

### The atomics in order

The whole build, with the operation and the memory order of every
atomic it does:

```
 BUILD   every participant
           tess_build_take_chunk       add, relaxed: a chunk number
           its chunk into its own list (plain: only it adds to the
             list until SIZE)
           tess_table_append           plain stores into its chunks
           tess_table_spill_*          acquire loads, release stores,
                                       acquire-release additions,
                                       ORs and swaps
           tess_build_report           add and OR, relaxed
 ═══ wait ═══ orders all of the above before what follows
 FLUSH   every participant writes its chunks of the partitions on disk
 ═══ wait ═══
 SIZE    the elected one
           tess_build_totals           loads, relaxed
           the index for exactly that many records (plain stores; the
             buckets cleared), the directory of the chunks by number,
             from every participant's list
 ═══ wait ═══
 LINK    every participant
           the chunks' addresses from the directory
           tess_table_link of its own  add to the count of records,
             chunks                    then a swap of each bucket,
                                       both acquire-release
           tess_build_add_duplicates   add, relaxed
 ═══ wait ═══
 OUTER   every participant writes its share of the outer side
 ═══ wait ═══
 PROBE   every participant probes      acquire loads of the buckets
           a RIGHT or FULL join marks  OR, acquire-release
 ═══ arrive and leave ═══ the last frees the index, the directory,
                          the chunks and the values
```

The orders of the link and the probe are those of [Calls at the same
time](../relhash/spec.md#requirement-calls-at-the-same-time) in the
spec of `relhash`, and [Several processes at
once](../relhash/design.md#several-processes-at-once) in its design
explains them.

## The words of a spill

A table that passes its node's memory splits into partitions and sends
some of them to disk, as "Partitions: a table that spills" explains. In
one process the node decides alone. In a shared table every participant
appends, and all of them must agree on two things. The first is the
number of partitions, since a record's partition is taken from its hash
by that number. The second is which partitions are on disk, since each
participant writes its own chunks of them.

The decisions lie in the **words of a spill**: an array of 64-bit
words, a head of five words and then five words for each partition. The
layout is drawn in [The words of a
spill](spec.md#requirement-the-words-of-a-spill). A shared table keeps
them in memory every participant maps, and changes them by atomic
operations. A join or a grouping of one process keeps the same words in
its own memory.

One code serves both. In Rust it is written once, over a trait of the
operations on a word, `Spill<W>` of `shared_spill.rs`, which has two
forms: atomic words, and the plain words of one process. The C calls
take `shared` to choose. So a process's own spill decides by the same
rule as a shared one, and the tests run the two side by side and
compare every answer.

### The split

The first participant whose bytes pass the budget splits the table. It
swaps the word of the partitions from 0 to its number, by a
compare-and-swap. Another one that wants to split at the same time
finds the swap failed, and takes the number the word holds. Say A wants
4 partitions and B, at the same moment, 8. A's swap comes first; B's
fails and reads 4; both use 4. The number is a power of two, since a
partition is taken from the low bits of the hash, and at most the
partitions the words were sized for.

### The bytes and the budget

A participant adds the bytes of every chunk it allocates to the word of
its partition and to the total, and takes them away when it frees the
chunk, `tess_table_spill_add_bytes`. The call answers whether the total
passes the budget; a total equal to the budget does not. A count that
would go below zero, or past 2^64 - 1, means that the node counted
wrong, and the call fails.

### The rule that sends partitions to disk

One rule, `tess_table_spill_evict`, chooses the next partition to send
to disk, for every node. Each call chooses one, marks it on disk and
returns it; the node writes what it holds of it and calls again, until
the rule returns none. The nodes differ only in the weights they give,
`TessSpillWeights`, whose exact rule is in [Sending partitions to
disk](spec.md#requirement-sending-partitions-to-disk). In short:

1. While the memory passes a share of the limit, the partition with the
   most bytes in memory goes. The share is `start` for the first
   partition of a check and `target` for each after it. The memory
   counts `reserve` bytes more for each partition on disk. A partition
   already on disk counts its bytes times `spilled`, and 0 leaves such
   partitions out.
2. Then, when some partition is on disk and those still in memory hold
   fewer than `resident` of the records, the lowest partition in memory
   goes.
3. A check sends at most `per_check` partitions, or any number for 0.

Each node gives its own weights, from its settings:

- **A grouping** starts at seven eighths of its memory and goes down to
  half (`tessera.agg_spill_start`, `tessera.agg_spill_target`). After a
  spill it must make its index anew, a pass over every record, so it
  sends several partitions at once. A partition on disk still takes new
  groups into memory, so it weighs as much as one in memory
  (`tessera.agg_spill_spilled_weight`, 1) and may go again.
- **A join of one process** sends partitions while its memory passes
  the limit (`tessera.join_spill_start` and `tessera.join_spill_target`,
  1). It reserves, for each partition on disk, the tail its outer side
  will need. Once a quarter of the inner rows or fewer are left in
  memory, it sends them all (`tessera.join_spill_resident_share`), since
  probing a few rows in memory costs every outer batch a whole probe.
  It leaves out the partitions on disk
  (`tessera.join_spill_spilled_weight`, 0).
- **A shared table** weighs every participant's chunks, which its words
  count, against its budget. A participant sends one partition a check
  (`tessera.join_shared_spill_evictions`, 1): the others write their
  chunks of it at their next batch, and the memory goes down only then.

For example, a grouping with a limit of 1000 kB holds four partitions
of 400, 300, 200 and 150 kB. The 1050 kB pass seven eighths of the
limit, 875 kB, so partition 0 goes, the largest. The 650 kB left still
pass half the limit, so partition 1 goes too. The 350 kB left are below
half, and the check ends; the grouping then makes its index anew, once,
over partitions 2 and 3.

### Marked once

In a shared table two participants may choose the same partition at
the same moment. The mark is an atomic OR of the partition's flag "on
disk". The participant whose OR set the flag adds 1 to the count of
partitions on disk, `tess_table_spill_evictions`, and gets the
partition. The other one finds the flag set and gets none, since the
partition went already. After each batch a participant compares the
count with the one it saw last, and when the count grew, it writes its
own chunks of the partitions now on disk.

### A partition chosen again

With `spilled` above 0, a partition on disk may hold the most bytes:
its tails, which take its new rows. The rule then returns it again, and
the node must free what it holds of it, or the next call chooses the
same partition. A grouping writes the groups it holds of it and frees
them. A join writes the values and the tail it holds of it to its files
and frees them, as it does when its level starts joining; when it holds
nothing of the partition, its check ends there, since what is left is
held by other participants. A join once wrote its empty tail and kept
it, freeing nothing, and asked again: with no limit a check, the loop
never ended, and it did not even notice a cancel. Its loops now also
check for interrupts.

### Records, starts and files

The words also serve the joining of the partitions on disk, after the
build:

- `tess_table_spill_records` counts the records of each partition, so
  that the elected one of SIZE can tell whether a partition fits in one
  participant's memory.
- `tess_table_spill_start` gives each participant the partition to
  start at: the next value of a counter, modulo the partitions. The
  participants then go round the partitions from different places.
- `tess_table_spill_take_file` hands out the files of a partition, its
  inner ones or its outer ones, each to one participant: it adds 1 to
  the partition's counter of such files and returns the value before.
  The slot after the last partition counts the outer files of the
  partitions kept in memory, which the participants read in PROBE.
- `tess_table_spill_take_alone` gives a partition whole to one
  participant: an atomic OR of the partition's flag "alone", which only
  one OR sets.

A count or a file number that would pass its word is refused, as a
count of bytes is.

## The rounds over a partition on disk

A partition on disk that fits in one participant's memory is joined by
all the participants together, in a **round**. A round has a barrier of
its own and five phases, which `tess_round_step` steps as
`tess_build_step` steps a build:

```
 phase        every participant                the elected one
 ──────────   ──────────────────────────────   ────────────────────
 0 ELECT      waits
              ═════════ all wait ═════════
 1 ALLOCATE   waits                            makes the partition's
                                               index
              ═════════ all wait ═════════
 2 LOAD       takes its inner files one at a
              time, loads each block into
              shared memory and links it
              ═════════ all wait ═════════
 3 PROBE      takes its outer files one at a
              time and probes; leaves without
              waiting
 4 FREE       the last to leave frees the partition
```

A link reads only its own chunk and the buckets. So a participant links
each block as soon as it loads it; the chunks the others load stay
empty ones to it until PROBE. A participant that comes late loads the
files still left at LOAD, probes with the outer files still left at
PROBE, and at FREE leaves at once and goes on to the next partition.

A partition too large for one participant's memory is not a round. One
participant takes it whole, by `tess_table_spill_take_alone`, and joins
it as a join of one process does: it splits the partition, or joins it
in pieces. The others go on to other partitions. So the many rows of
one key hold up one participant, not all of them.

A participant that attaches to a build that spilled only when the build
is over, at FREE, leaves at once and joins no round. The other
participants join every partition, so the rows are right; only that
participant's help is lost. It happens under a parallel append, whose
workers take a partial child that others are still reading: a worker
that finished another child comes to the join while its participants
are in the rounds. That worker then goes on to the next child, so its
help is lost only when the join is the last child left. "A late
participant in the rounds", under "What we decided not to do", says why
it stays so.

### A partition read back that splits

A partition read back that is still too large splits into a level
below, as "A partition too large for memory" says. Whether it does is
one rule, `tess_table_spill_splits`, with two weights a node gives,
`room` and `key`; its exact form is in [A partition read back that
splits](spec.md#requirement-a-partition-read-back-that-splits). For
example, a join with a limit of 900 kB, 300 kB in use and a `room` of
two thirds splits a partition of 500 kB, since 500 passes two thirds of
the 600 left. It does so only if its level leaves two bits of the hash
for the level below, and if the partition holds fewer than nine tenths
of its level's rows; more is one key, which no split parts.

## What the loom model shows

`make rust-loom` runs the table's own Rust code many times, in every
order of the steps of its threads that the memory model allows. Loom
is the tool that does this. Every access to the bytes of a record is
announced to loom first, so a read that is not ordered after its
writing is reported. For a shared build the model adds PostgreSQL's
barrier: the same counts, phase and election, with a mutex for its
spinlock and a condition variable for its wait. The models of this part
are:

- two participants that append, size, link and probe one table, and
  three that attach at any phase: every key is found, the chunks are
  numbered from 0 once each, every record is counted, and the table is
  freed once;
- two participants past the budget that split at once: they agree on
  one number;
- two that send the largest partition to disk at once: it is marked
  and counted once;
- two that take the files of a partition, and the partition whole, at
  once: each goes to one of them;
- rounds of two and three participants that attach at any phase: every
  file is loaded once, every key is found, and the round is freed once.

A model that checks nothing would pass too. So the model has negative
tests, each a mistake that it must report. There are eight: bucket
heads read relaxed let a probe read a record not yet written; a count
of records raised only after a record is published makes a sound chain
look like a loop; a filter's state stored relaxed lets a reader see the
filter unfilled; a plain read and write in place of an atomic OR loses
the bits of a filter, and the marks of a RIGHT or FULL join; a stop
marked after leaving goes unseen; linking before the index is made
breaks the table; and a round that probes before every file is loaded
misses keys.

The model differs from the node in one place. A participant of the
round model links its files once, after it loads them all, where the
node links each block as it loads it; the order the model checks, every
link before the barrier of PROBE, is the same.

## What we decided not to do

- **Waits in Rust.** The step of a shared build could wait at the
  barrier itself, through a function the node gives it. But a wait may
  raise a PostgreSQL error, which would jump over the frames of Rust
  functions. The waits stay in the node, and the step only says what
  to do next.
- **A rule of eviction in each node.** The grouping, the join and the
  shared table each had a rule of their own in C that chose the next
  partition to send to disk. They became one rule with weights, so that
  a fix serves every node and the tests of the one rule cover all.
- **A late participant in the rounds.** PostgreSQL's parallel hash join
  keeps every participant on its build's barrier until all its batches
  are done, so its last phase means that no work is left. Tessera's
  participants leave the build's barrier once the part in memory is
  probed, so that the last one frees that part before the rounds take
  memory; a participant that comes after that finds no work, though the
  rounds still run. Two ways would let it help. It could go round the
  partitions on disk as the others do: a path that only a participant
  late by just that much takes, which no test reaches without a way to
  hold a worker back, and late paths are where a shared join once
  crashed. Or the participants could stay on the build's barrier as the
  core's do, with the part in memory freed through a barrier of its
  own; that moves the return of a RIGHT or FULL join's records without
  a pair, and the stop of a participant that leaves while it probes, to
  the new barrier. Both cost more than the help they win.

## What is not on this page

The same index, chunks and records serve more than a plain table. These
parts are described in [docs/table.md](../../../docs/table.md) until
they get their own pages:

- the aggregate states that a grouping keeps in a payload, and the
  calls that fold rows into them;
- the items a sort makes from records.

How a spilled chunk of records is written to disk is in
[spill-format](../spill-format/design.md).

## Files

- `crates/relhash/src/table/`: the table itself, whose files the
  design of [relhash](../relhash/design.md) lists
- `crates/tessera-kernels/src/column_keys.rs`: the keys read from
  Tessera's columns
- `crates/tessera-kernels/src/phases.rs`: the participant of a
  shared build and of a round, the counters of a build and the stop
  word
- `crates/tessera-kernels/src/shared_spill.rs`: the words of a
  spill, the split, the rule that sends partitions to disk, the
  counters of the rounds and the rule of a split read back
- `crates/tessera-kernels/src/loom.rs`: the models of a shared build,
  of the rounds and of the words of a spill
- `crates/tessera-capi/src/c/table.rs`: the C entry points
- `crates/tessera-capi/src/c/shared_spill.rs`: the C entry points of a
  spill and of the rounds
- `crates/tessera-spill/src/plan.rs`: the plan of a level, its
  partitions and the length of its chunks
- `crates/tessera-capi/src/c/spill.rs`: the C entry points of the plan
  of a level, beside those of the format of a spill
- `include/tessera/table.h`, `include/tessera/table_key.h`: the C API

## Tests

- The unit tests in `crates/relhash/src/table/tests/`: the format and
  every call, with damaged headers, chunks, references and chains, over
  keys of their own; they also run under Miri, at smaller sizes.
- The unit tests in `crates/relhash/src/table/tests.rs`: blocks
  that are not chunks or an index, partitions, merges, and a shared
  filter that threads race to build.
- `crates/tessera-kernels/tests/table.rs`: keys read from columns, and
  the states of groups folded into records and merged.
- The unit tests in `crates/tessera-kernels/src/accumulate.rs`: the
  states of groups merged by their kinds, and a count past the int8
  range.
- The unit tests in `crates/tessera-kernels/src/phases.rs`: the
  actions of a participant alone, attached at every phase of a build
  and of a round, and the counters of a build under threads.
- The unit tests in `crates/tessera-kernels/src/shared_spill.rs`:
  the split, the budget, the weights of each node, a process's own
  words against shared ones, and the counters of the rounds.
- `crates/relhash/src/table/loom/` and
  `crates/tessera-kernels/src/loom.rs`, run by `make rust-loom`.
- The unit tests in `crates/tessera-spill/src/plan.rs`: the plan of a
  level on examples, against the nodes' former rules, and the arguments
  it refuses.
- `crates/tessera-capi/tests/table.rs` and
  `crates/tessera-capi/tests/spill.rs`: the entry points as C calls
  them, Datum and dense key columns alike, and the arguments they
  refuse.
- `test/sql/table.sql` with `test/tessera_table_test.c`: the C API
  from a PostgreSQL backend, and the check of the layout when the
  kernels load.
- `test/sql/join.sql` and `test/sql/agg.sql`: partitions as the nodes
  use them. A join and a grouping that spill, with the core's results;
  partitions read back that split into a level below, in a join
  (`jwide`) and in a grouping (`agg_rows`); a join's partition of one
  key joined in pieces (`jskew`), and partitions joined in pieces when
  a setting forbids the split; groups merged from several writes to
  disk.
- The benchmarks `table_int32` and `table_large` of `tessera-bench`
  count the instructions and cycles of append, probe and find-or-insert;
  `table_large` also those of the Bloom filter's check.
