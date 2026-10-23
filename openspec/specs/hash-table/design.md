# hash-table: how it is built

A hash join and a grouping both need to find rows by a key fast.
Tessera keeps such rows in a hash table of its own. The table is
written in Rust, in the kernels, and the nodes in C call it through
`tessera/table.h`.

This page explains what the table is made of, how a batch of rows goes
in and how it is looked up, how the table grows, and how several
processes use one table at once. It then explains what a join and a
grouping keep beside the table: partitions when the table spills to
disk, a Bloom filter of its keys, and the marks of a RIGHT or FULL
join. Last, it explains how the processes of a parallel join build one
table together, step by step, and agree on which of its partitions go
to disk. The exact bytes, limits and errors are written once, in the
table's spec, [spec.md](spec.md) next to this page. Where this page
leans on a rule of the spec, it links to the requirement that states
it. What a join or a grouping does with the table is described with
those nodes.

## The problem

A table in a database engine has to answer some hard questions. These
are the ones Tessera's table answers.

1. A plan that runs in one process keeps a node's memory in a
   PostgreSQL memory context. A parallel hash join keeps the build side
   in dynamic shared memory, which every process maps at an address of
   its own. An address that one process stores means nothing to
   another. How can one table, and one code, serve both?
2. The planner guesses how many rows will come, and the guess can be
   wrong by a hundred times. How does the table grow without copying its
   records, and without holding an old and a new copy at once?
3. In a parallel join several processes put rows into one table at the
   same time. How do they do it without a lock? How does a process that
   finds a record know that the record is whole?
4. The table's memory is bytes that a node hands over. A wrong call or a
   bug elsewhere may leave wrong bytes there. How does the table avoid a
   crash of the server, or a loop that never ends?
5. A join needs every record of a key. A grouping needs exactly one
   record of a key, made the first time the key comes. How does one
   table serve both?
6. A table larger than the processor's caches makes almost every look
   at a record a wait for memory. How can a batch of lookups hide these
   waits?
7. Rust code must not raise a PostgreSQL error, and memory it allocated
   would be memory PostgreSQL does not count. How does a call say "this
   chunk is full" or "the buckets are too few"?
8. A table may not fit in the memory a node is given. The node then
   writes some records to disk and reads them back later, a part at a
   time, and a part read back may still be too large. How does the
   table sort its records into such parts?
9. In many joins most rows of the probe side find no pair, and each
   still costs a read of its bucket, a wait for memory on a large table.
   How can such a row be dropped without reading the table?
10. A RIGHT or FULL join also returns the records that no row matched.
    The records are shared and read by every process. Where does the
    join keep which records found a pair?
11. In a parallel join every process builds a share of one table, and
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

- **No address inside the table** (question 1). The caller allocates
  every block of the table and hands the blocks to each call. A record
  is found by a *reference*: the number of its block and its place in
  the block, 32 bits in all. A process turns a reference into an address
  through its own list of the blocks. The same bytes then mean the same
  in every process. The price is one more read per record, of the
  block's address from that list, which is almost always in the
  processor's fastest cache.
- **Records never move** (question 2). Records lie in blocks called
  chunks. When a chunk is full, the caller adds another. The buckets
  are made for a number of records, the table's capacity. A grouping
  that meets more groups than the planner expected fills its buckets:
  when its records reach half the buckets, the chains would start to
  grow long. Then only the array of buckets is made anew, larger, over
  the same chunks (see "One writer"). A join never needs this, since it
  makes its buckets once, when it knows how many rows it has. The
  price, against an older form of the table that kept everything in one
  block and copied it to grow: about 6 to 7 percent more instructions
  for a lookup in cache, 22 to 30 percent more for an insertion, and 2
  to 4 percent more time for a join in SQL. The gain: under an estimate
  200 times too low, a join used 13 to 34 percent less memory, since
  nothing is ever held twice.
- **No lock** (question 3). A process writes records only into chunks
  of its own. It then makes each record visible with one atomic
  compare-and-swap on the record's bucket. A process that reads the
  bucket with the matching memory order sees the whole record. The same
  code runs in a plan of one process, where the compare-and-swap never
  fails. Its price there was measured too, and it was not the main cost
  of an insertion.
- **Wrong bytes are an error, not a crash** (question 4). Every call
  checks the whole header of the table. Every reference is checked
  against its chunk before it is followed. A walk down a chain takes at
  most as many steps as the table has records, so a chain that loops is
  found. The price is a fixed part of every call, a few hundred
  instructions, and one comparison per step.
- **Two ways to put a row in** (question 5). A join *appends* rows,
  and equal keys make separate records. A grouping asks to *find or
  insert*: each row gets the one record of its key, which is made when
  missing.
- **Lookups by batch** (question 6). One call looks up all the rows of
  a batch. When enough rows are selected, the call goes step by step
  over all of them together: it reads every row's bucket, then every
  row's first record, and so on, asking the processor to fetch the next
  memory early. The waits of different rows then overlap. On a table far
  larger than the caches a lookup that finds its key went from about 75
  to about 38 processor cycles a row. On a table that fits in the
  fastest cache it costs about half as much again as a lookup of one
  row at a time. That price was accepted: the tables of real joins are
  larger than the cache.
- **Nothing kept, nothing raised** (question 7). Rust keeps nothing
  between calls, and the table's memory is all the caller's. A call
  allocates nothing but the text of an error; the list of the columns of
  a payload, up to 2048 of them, is kept on the stack. A row that does
  not fit stays in the call's mask of rows still to place; the caller
  adds a chunk, or makes a larger index, and calls again. Every error is
  a status that the call returns, and the caller raises it as a
  PostgreSQL error after the call.
- **Short chains.** The table has at least twice as many buckets as the
  records it was made for, and at least 1024. A grouping stops making
  records when they reach half the buckets. The price is the buckets: 8
  bytes for each record the table was made for.
- **Parts by the low bits of the hash** (question 8). A table that
  spills keeps its records in partitions, chosen by the low bits of the
  hash already in each record, each with chunks of its own. A part read
  back that is still too large is read a chunk at a time and split by
  the next bits, so it never has to fit in memory whole. The price is a
  copy: the records built before the table spilled are copied once into
  their partitions, and once more at each split.
- **A filter checked with one read** (question 9). A join may check its
  rows against a Bloom filter of the table's keys first. Each key sets
  four bits of the filter, and all four lie in the same word of 64
  bits, so checking a row reads one word. The price is 16 bits a record
  at least, and the rows that pass falsely, about one in 200 at 16 bits,
  which then probe the table as before.
- **Marks beside the records** (question 10). A RIGHT or FULL join keeps
  a bit for each record in words of its own and sets the bits of the
  records its pairs matched, by an atomic OR in a shared table. The
  records stay as they were written. The price is a bit a record, and
  in a shared table the words of a full chunk for every chunk.
- **Steps that never wait** (question 11). A participant of a shared
  build is a small state machine of the table's C API. Each call
  returns the next action, such as build, wait, link or probe; the node
  does it, waits included, and passes what the wait returned to the
  next call. What the participants decide together, the split, the
  partitions on disk and the files each one reads, lies in words every
  one of them maps, changed by atomic operations, and the first one to
  change a word decides for all. The price is a call for each step,
  and a loop in the node that must follow the actions in their order.

## The index

The nodes size the index in different ways. A join in one process
appends its whole inner side first and then makes the index for exactly
the rows it got. A grouping makes its index for the planner's estimate
of the groups, no larger than its memory allows, and moves to a larger
one when its records reach half the buckets (see "One writer"). A
parallel join makes the index once, after all processes have appended
(see "Putting rows in").

## Chunks and records

The nodes make the first chunk of a table small, 64 kB, so that a
table of a few rows takes little memory. Later chunks are larger: an
eighth of the node's memory limit, but at most 1 MiB, the limit of a
chunk. A node may also pass a chunk of 8 bytes, its used mark alone,
where it has no chunk to give for a number: nothing fits in it, so
nothing is ever written there.

### Values longer than a word

All records of a table have one size, so a record cannot hold a value
whose length varies, such as a string. A key never needs to: a key of
such a type goes in as a number of 64 bits (see "Keys of each SQL
type"). A
payload keeps such a value outside the table. The node copies the
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

## References

A node keeps its tables far from that limit. A join or a grouping
sends rows to disk when its memory reaches `hash_mem`, and a sort when
it reaches `work_mem`; none of them counts its chunks. Each takes its
limit through `tess_table_memory_limit`, which keeps it at most 16 GiB,
half of what the chunks can hold, so that the node sends rows to disk
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
  [The index of a table](spec.md#requirement-the-index-of-a-table)
  lists them. The calls that append to chunks before an index exists
  have no header to check.
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
[Errors of a call](spec.md#requirement-errors-of-a-call). The C node
raises the error with `ereport` after the call returns, because a
PostgreSQL error must not unwind through Rust code.


## Partitions: a table that spills

A node is given a limit of memory. When its table would pass the limit,
the node keeps the records in **partitions** and writes whole chunks of
some partitions to disk, to read them back once its input is done. How
a chunk is written to disk is in
[spill-format](../spill-format/design.md); when a node spills is the
node's. This section is about how the table sorts its records into
partitions, and how many partitions a level gets.

The partition of a record is taken from the low bits of its hash: with
4 partitions, the two lowest bits. A partition read back that is still
too large is split again by the next bits up. The rule, with its
limits, is in
[Partitions of a table](spec.md#requirement-partitions-of-a-table).
For the hash `0x3C27`:

```
 bit           …  7 6 5 4   3 2   1 0
 hash 0x3C27   …  0 0 1 0   0 1   1 1
                            └┬┘   └┬┘
                             │     └─ first, 4 at shift 0: 0b11 = 3
                             └─ split again, 4 at shift 2: 0b01 = 1
```

The record goes to partition 3; if partition 3 is split later, it goes
to partition 1 of that split. The hash is in the record, so nothing is
computed again. The low bits are taken because the buckets take the
high ones: a partition read back gets an index of its own, and its
records then spread over all of the buckets. Had the partitions taken
the high bits, every record of a partition would share them, and all
of them would fall into a few buckets.

A split is not limited to two levels. Each split makes a **level**: its
partitions take the bits just above those of the level it splits. A
node makes 4 to 1024 partitions a level, so a level takes 2 to 10 bits.
A partition of any level splits again when it is still too large, as
long as two of the hash's 32 bits are left, so the levels can go on to
the length of the hash. Each level divides a partition's records among
its partitions, so a few levels are enough even for a table far larger
than the memory. What no split helps is many rows of one key: they
share the whole hash, and every level puts them in one partition.
"A partition too large for memory" below says what the nodes do then.

A partition is a list of chunks. Each call that works by partition gets
one chunk number for each partition: the chunk that partition appends
to now.

```
 partition   its chunks                       partition_chunks
     0       chunk 4 (on disk), chunk 9          9
     1       chunk 5                             5
     2       none yet                            1  (the empty chunk)
     3       chunk 7 (full), chunk 8             8
```

A row can be written only where its partition's chunk has room. A
partition that has no chunk yet, partition 2 above, names the **empty
chunk**: a chunk of 8 bytes that holds its used mark alone and has no
room for any record. To the call it is just a full chunk, so the call
needs no case of its own for a partition without a chunk. Nothing is
ever written to the empty chunk, so any number of partitions may name
the same one.

When a row's chunk has no room, the call does not stop. It leaves that
row in its mask of pending rows, the rows it did not place, and goes on
with the next ones, so one full partition does not hold up the others.
The node then gives each partition with a pending row a new chunk and
calls again for the pending rows. The calls by partition need no index.
When the table has one, as a grouping's has, they check that the
records they write have its keys and its payload, as an append does,
and refuse others.

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

In many joins most rows of the probe side find no pair. Such a row
still costs a read of its bucket, and on a table far larger than the
caches that read waits for memory. A **Bloom filter** answers "surely
not here" from far less memory. It is an array of bits. Each key of the
table sets a few of them, chosen by its hash; a row whose bits are not
all set has no key in the table. A row whose bits are all set may still
have none, a false pass, and then probes the table as it would anyway.

A plain Bloom filter sets its bits anywhere in the array, so a check
reads several places, each a possible wait. Tessera's filter is
*blocked*: the four bits of a key lie in one word of 64 bits, so a
check is one read and one comparison. The rule is in [A Bloom filter of
the keys](spec.md#requirement-a-bloom-filter-of-the-keys); here it is
on an example.

**The size.** A table of 1000 records wants 16 bits a record: 16 000
bits, or 250 words. The words are rounded up to a power of two, 256
words or 2 KiB, so the filter has about 16.4 bits a record. The power of
two lets the word be found by a shift: 256 is 2^8, so the number of a
word is 8 bits long.

**The word and the bits.** The hash of a key, here `0x3C27`, is
multiplied by the odd constant `0x9E3779B97F4A7C15`, and the low 64
bits of the product are kept. The top 8 bits of the product are the
number of the word. The low 24 bits are four numbers of six bits each,
and six bits name one of the 64 bits of a word.

```
 hash 0x3C27 × 0x9E3779B97F4A7C15 = 0x1AFB0517D96DD333

 bits of the product  63 … 56   55 … 24    23…18   17…12   11…6    5…0
                      00011010  not used   011011  011101  001100  110011
                      word 26              27      29      12      51

 filter     word 0   word 1   …   word 26   …   word 255
                                     ▲
            the key sets bits 12, 27, 29 and 51 of word 26
```

A row of the probe side is checked the same way: its hash gives a word
and four bits, and the row passes when all four are set in that word. A
row with the hash `0x3C27` reads word 26 and finds the bits this key
set. A row of another hash passes without a pair when other keys
happened to set all of its bits: a false pass. Two of the four numbers
may be equal, and a key then sets three bits.

**Why the product.** The bucket of a key is taken from the high bits of
the hash itself. Had the word been taken from those bits too, the keys
of neighbouring buckets would share a word and crowd it. The product
mixes every bit of the hash into its high bits, so the word does not
follow the bucket.

**How many pass falsely.** At exactly 16 bits a record, 484 of 100 000
absent keys passed a filter of 4096 keys, about one in 200. Rounding up
to a power of two adds bits, and fewer pass. The size and the measured
bound are in [The size of a Bloom
filter](spec.md#requirement-the-size-of-a-bloom-filter).

The join uses a filter in three ways. When it wants one, and whether it
is worth its cost, the join decides by its own rows.

- **A table of one process.** After the build, `tess_table_bloom` fills
  a filter from every record of the chunks, and `tess_bloom_probe`
  checks each batch before the probe.
- **A shared table.** The filter is shared too, behind a state word.
  Each participant decides by its own rows whether it wants the filter,
  and the first that does builds it alone; the others go on without it
  until it is ready. Nobody waits.

  ```
   none ──first participant's compare-and-swap──► building
                                                     │ fills the filter
                                                     ▼
   ready ◄──────────── a store with release ─────────┘
     │
     └─► a participant that reads ready, with acquire, checks its rows
         against the filter; before that, it probes the table alone
  ```

  The acquire read of ready makes every bit the builder set visible, as
  the read of a bucket makes a record visible. The participant that
  makes the index allocates the filter's words with the table, 16 to 32
  bits a record of the query's shared memory, but only when a
  participant may want them: at the default rule, for a table of 4096
  records at least, since a smaller one stays in the cache, where a miss
  costs less than the check.
- **A table that spills.** A filter of every row of the build side is
  filled as the rows come, before some of them go to disk: by
  `tess_bloom_add` in one process, or by every participant at once by
  `tess_bloom_add_atomic`, which sets the bits of a word by an atomic
  OR. Rows of the probe side that it rejects have no pair in any
  partition, and are dropped before they are written to disk. A
  barrier orders every addition before the first check. This filter is
  sized within an eighth of the node's memory: it lets more rows through
  when it is small, but leaves the memory to the rows.

The loom model checks the shared filter as it checks the buckets: two
participants race to build it and one does, and a participant that
reads ready sees every key pass. In negative tests, a state stored
relaxed lets a reader see a filter not yet filled, and an addition by a
plain read and write in place of the atomic OR loses bits.

The calls check the count of a filter's words and of a batch's rows.
The words of a filter in one process are an array of the C API like any
other, whose alignment the C type already requires; the calls over a
shared filter, or one filled together, check that the words are aligned
to 8, since they read them as atomic words.

## The marks of a RIGHT or FULL join

A RIGHT join returns every row of its build side, the side the table
holds, and so does a FULL join: the rows that no row of the probe side
matched come out after the probe side, with NULL for its columns. The
join must therefore know which records found a pair. It keeps a **mark**
for each record, one bit, in words of its own beside the chunks: a run
of words for each chunk, bit `i` of word `w` for the chunk's record
`64 w + i`. The layout is drawn in [The marks of RIGHT and FULL
joins](spec.md#requirement-the-marks-of-right-and-full-joins).
With records of 32 bytes, the reference 131353 names chunk 1, byte
2248; that is the chunk's record (2248 - 8) / 32 = 70, so its mark is
bit 6 of word 1 of chunk 1's run.

```
 probe:   a pair passes the join's conditions
            └─► tess_table_mark: the bit of the pair's record is set
 then:    the probe side is done
            └─► tess_table_next_unmarked: the records whose bits are
                clear, a batch at a time, from a cursor; a count of 0
                ends the walk
```

The marks are not kept in the records. The records are read by every
participant while they probe, and stay as they were written. The walk
reads the marks of 64 records in one word, and reads no record whose
bit is set. The walk needs only the chunks, so the calls take the size
of a record from the caller and check it against the index when the
table has one.

In a shared table the marks lie in shared memory, a run sized for the
largest chunk for every chunk, and two participants may mark records of
one word at the same time; each word is changed by an atomic OR, so no
mark is lost. The last participant to leave the table walks the records
without a mark. The barrier it leaves through orders the marks of every
other participant before its walk. The loom model marks records of one
word from two participants, and a negative test shows that a plain read
and write loses marks.

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
time](spec.md#requirement-calls-at-the-same-time), and "Several
processes at once" above explains them.

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

- `crates/tessera-kernels/src/column_keys.rs`: the keys read from
  Tessera's columns
- `crates/relhash/src/table/bloom.rs`: the Bloom filter, alone,
  shared and filled together
- `crates/relhash/src/table/marks.rs`: the marks of a RIGHT or
  FULL join and their walk
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
- The unit tests in `crates/relhash/src/table/marks.rs`: the
  words of the marks, and the records and cursors a walk refuses.
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
