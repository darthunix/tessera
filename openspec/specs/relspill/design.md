# relspill: how it is built

When a hash join's table, a grouping's groups or a sort's rows outgrow
the memory of a query, some of them go to temporary files and come
back later: they spill. The crate `relspill` holds what a spill
decides and what it writes: when it starts, which partition goes to
disk, how a level is planned, the bytes of the files, and the checks of
those bytes when they come back. It is written in Rust, reads and
writes no file, takes all its memory from its caller, and depends on
nothing of Tessera but its hash table, `relhash`. Tessera's C nodes
call it through `tessera/spill.h` and `tessera/runtime_spill.h`.

This page explains what a spill is and the problem the crate solves,
the goals it keeps and what each costs, and the whole in one picture.
Then it explains each part: the crate and its host, the file of a set,
packing, the words of a spill and its rules, the plan of a level, the
machines and how a host puts them together, and the errors. Last, it
explains what shows that bytes which are not the crate's give an error
and never a crash. The exact bytes, limits and rules are written once,
in the documentation of the crate's items, which the crate's spec,
[spec.md](spec.md) next to this page, links to; where this page leans
on a rule, it names the item that states it. How Tessera's nodes use
the crate is explained in the designs of
[hash-table](../hash-table/design.md) and
[spill-format](../spill-format/design.md).

## Background: partitions, levels, runs

A hash join builds a hash table from one input, the inner rows, and
looks up each row of the other input, the outer rows, in it. When the
table grows past its memory limit, the join spills:

1. The hash of each row picks one of the level's partitions.
2. Some partitions stay in memory. The others are written to a
   temporary file, a chunk at a time, and so are the inner rows that
   come later for them, and the outer rows of such a partition.
3. When the inner rows end, the outer rows probe the partitions in
   memory first. Then each partition on disk is read back and joined
   alone.
4. A partition read back that fits is joined whole. One that does not
   is split into a level below, by other bits of the hash. When it
   cannot be split, because no level fits in the bits of the hash left
   or most of its rows share one key, it is joined in pieces: one piece
   of its inner rows is loaded, all its outer rows are read against it
   (one pass), and the next piece follows.

```
                          rows in
                             |
                    hash picks a partition
                             |
             +---------------+---------------+
             |                               |
         in memory                    on disk: a set,
             |                        one temporary file
       finished first                        |
                                 read back one at a time
                                             |
                  +--------------------------+-----------------+
                  |                          |                 |
           fits: handled           too large: split    cannot split:
           whole                   into a level below  joined in pieces
```

A grouping keeps one record for each group, its keys and the states of
its aggregates, in a hash table. When the table passes its limit, the
records of its largest partition go to disk whole; new rows of those
groups then make new records in memory, which merge with those on disk
once the input ends. A grouping whose aggregates' states cannot merge
spills another way: its table stops taking new groups, and the rows of
the groups it lacks go to disk, to be grouped later in a table of their
own.

A sort spills a third way. It sorts what fits in memory and writes it
out as one sorted run. At the end it merges the runs, in several passes
if there are many.

In a parallel query, several processes can build one hash table in
shared memory. Each of them is a participant. They go through the
phases of the build together, and wait for each other at barriers
between the phases. After the build, the partitions on disk are handled
in rounds, all participants on one partition together, or by one
participant alone.

## The problem

A spill is many small decisions, each made by numbers: the bytes a
table holds and its limit, the rows of a partition, the bits of a hash
left. A join, a grouping, a sort and a shared build each make them, and
where each operator makes its own, the same rule is written several
times and its copies drift apart. A spill also reads back bytes it
wrote, and code that trusts them crashes or reads past its memory when
they are damaged.

So the decisions of a spill, and its files, belong in one place that
every operator uses, that a test can reach without a database, and
whose reads of a file can be shown to be safe whatever the file holds.
That place must not call the database: a call of PostgreSQL may raise
an ERROR, which jumps over every frame in between, and Rust's frames
may not be jumped over. And it must cost nothing on the path of a row.

## Goals and what they cost

- **A spill without Tessera.** A database engine other than Tessera can
  spill with `relspill` and `relhash`. Its host gives the memory and
  does the file input and output. The cost: every action the crate
  needs goes back to the host as a step to do and comes back as an
  event, which the host serves in a loop.
- **No input or output, no allocation.** The crate opens, reads and
  writes no file and allocates no memory: a call that needs a larger
  region says how large and changes nothing. The cost: the host keeps
  every region, the write buffer, the block lists, a reader's page and
  block, and the words of a spill and of every machine, and grows them
  when a step asks.
- **Every rule written once.** When a spill starts, which partition
  goes, how a level is planned, where a piece ends, which partitions a
  participant takes: each is the crate's, and no node keeps a copy. The
  cost: a node's decisions become a machine's steps, and the state that
  was the node's becomes the machine's words.
- **Any bytes, a bounded read.** Whatever bytes a file holds, a read
  gives the data or an error, ends, and asks for no byte past its file
  or the bounds its caller gave. The cost: every read checks the bytes
  it takes, behind one layer, and every block names its length in its
  header.
- **No cost to Tessera.** A loop over rows compiles no longer through
  the crate than it would in C. The cost: a machine steps once for a
  chunk, a block or a partition, never for a row, and the crate's loops
  over rows, packing a chunk, appending rows to a chunk of columns,
  reading a window of them, are kept as tight as hand-written ones and
  checked by their machine code.

The crate builds for targets whose pointers are 64 bits wide, as
`relhash` does. Its library is `no_std`: its feature `alloc`, on by
default, gives one helper over chunks each in a `Vec`.

## The whole in one picture

```
   host (Tessera's C nodes, or another engine)
   ------------------------------------------------------------------
   memory      files        barriers      rows, keys, values
     |           |              |                  |
     |   ranges to write,       |                  |
     |   bytes to read          |                  |
     v           v              v                  v
   +------------------------------------------------------------------+
   | relspill                                                         |
   |                                                                  |
   |  machines:  join   grouping   grouping's rows   sort's runs      |
   |             shared build (its phases and rounds)                 |
   |      |  plan of a level      words of a spill, their rules       |
   |      |                                                           |
   |  file of a set: writer (buffer, lists, trailer), reader (pages,  |
   |    blocks, checks)                                               |
   |  format: block header, packed records, chunks of columns,        |
   |    packed columns, references to values                          |
   +------------------------------------------------------------------+
                              |
                              v
           relhash: chunks, records, partitions of a table
```

A node holds the words of its machine and steps it with what it saw;
each step answers what to do next. When the answer is to write a
partition's chunk, the node gives the chunk to its set's writer, which
packs it and answers with the ranges of bytes to write and where; the
node writes them with its own file calls and reports them written.
When the answer is to read a partition back, the node opens a reader,
which answers with the bytes to read, checks each part before it asks
for the next, and gives the blocks back. The words of a spill count
the bytes and records of each partition, in one process or in memory
several processes share, and the rules over them name the partition
that goes to disk and say whether one read back splits.

## The words used below

- **Block.** What one write puts on disk: a header and a body, the
  chunk stored as it is or packed. A block holds records (the rows of a
  `relhash` table), columns (rows stored column by column), or values
  (the bytes of values by reference).
- **Chunk.** A group of rows in memory. `relhash` keeps a table as
  chunks of records; a sort or a join's outer side keeps rows as chunks
  of columns.
- **Level.** One split of the rows into partitions. Level 0 is the
  first; a partition split again makes a level below. The plan of a
  level says how many partitions it has, how long its chunks are, and
  which bits of the hash it takes.
- **Set.** The temporary file of a level: the blocks of all its
  partitions, in the order they were written, and a list of each
  partition's blocks. A shared set is one such file for each
  participant, which the others read. The end of a set's file, its
  trailer, says where its lists are.
- **Fingerprint.** A number every block and trailer of a set carries,
  which its host chooses to name what the chunks hold, such as the
  layout of a table's records: a reader of another fingerprint refuses
  the bytes.
- **Words of a spill.** The numbers a spill keeps for each partition,
  its bytes, its records and its flags, and its budget of memory. When
  several processes share a build, these words lie in shared memory.
- **Host.** The program that uses the crate. It reads and writes the
  files, gives memory, waits at barriers and reports errors.
- **Machine.** A value that remembers where the protocol of one node's
  spill is: the host gives it an event, and it returns the next action.
- **Wrong call, damaged bytes.** The two kinds of errors of the crate.

## The crate and its host

The crate never opens, reads or writes a file. Its host does, and tells
the crate what happened: a protocol is a value that takes bytes and
events, and gives back bytes and requests.

```
   node (C)                         relspill
   --------                         --------
   partition 3's chunk     ------>  join machine: step(Full)
   is full                 <------  Flush: write what it holds
   chunk of partition 3    ------>  set writer: write
                           <------  ranges: buffer bytes 0..8192
                                    to the file at 81920
   FileWrite at 81920
   written                 ------>  set writer: written
```

The line between the crate and its host: a rule that a database engine
without PostgreSQL would need in order to spill with `relhash` goes into
the crate; a rule of SQL, of PostgreSQL's types or of its executor stays
in the host.

Every call of PostgreSQL so stays in C, and no ERROR jumps over a frame
of the crate. The crate can also be tested whole: in a test a file is a
vector, a fuzzer feeds it any bytes, a model runs it in memory, and a
prover reasons about it without a file system.

- **Refused: callbacks for input and output.** Simpler for the crate,
  but an ERROR inside a callback would jump over the crate's frames,
  and every callback would need a guard of its own.
- **Refused: the files in the host.** The trailer and the lists would
  be a format that only the host writes and checks, outside every
  proof and fuzzer.

A call allocates nothing. Every region it uses comes from the caller:
the write buffer, the block lists, a reader's page and block, the words
of a spill and a machine's own. A call that cannot start without a
larger region returns how much it needs and changes nothing. A call
that appends rows to a chunk of columns may make progress first: it
appends the rows that fit, says which, and returns what the next row
needs, so that its caller can write the chunk out and go on.

So the memory of a spill is allocated where the host counts memory,
and a machine asks its host for the measure it decides by. The host
counts each region by its capacity, and only once. Some of a spill's
memory grows with its input: the block lists, the marks of the outer
rows that matched in a join read in pieces, and the block a join reads
ahead past a piece. A fixed reserve cannot hold memory that grows, so
what may pass the limit, and by how much, is a rule of its own, which
the spec of `table-memory` holds.

An action is outstanding from the step that returns it until the host
reports it done; while a write is outstanding, the crate takes no call
that would change the bytes it names, and the host replaces a region
only when no action holds it. An action can fail: a write cut
short, a read of fewer bytes, an error of the file manager, a limit of
temporary files, a cancel. The crate does not retry. The host reports a
write that failed to its set, which then takes only a reset; a machine
takes no such report, and its host ends the spill. In a shared build, a
participant whose action failed never arrives at a barrier as done,
and its claims in the shared words are not taken back: its host ends
the whole parallel query, as PostgreSQL ends the others when one
process raises an ERROR. A host that is not PostgreSQL must do the
same.

The host keeps three ways of ending a set apart. Releasing a set closes
its file and keeps it for the others. Freeing a set deletes its file.
Resetting a shared set for a new execution deletes every file of the
set; it comes only after every reader has let go, and it may reuse the
names of the last execution's files.

Three cases look alike and are kept apart:

- the file ends before a block ends: damaged bytes, an error of the
  crate;
- a participant wrote no file: its set is empty, which the host may
  say only once that participant has finished;
- the host could not do a read or a write: the host's own error, which
  ends the spill.

Several processes share two kinds of words, ordered two ways. The words
of a spill use acquire and release, so that a participant that sees a
partition on disk sees what came before it, as `shared_spill`
documents. The counters of a shared build and the word by which a
participant stops are relaxed: the host's barrier orders them, since
every write a participant made before it arrives at the barrier is seen
by every participant after they leave it, as `phases` documents. The
filter of a shared build is published under a lock before the split is
agreed.

## The file of a set

A set is one file that holds the blocks of every partition of a level,
in the order they were written, and at its end a list of each
partition's blocks and a trailer.

- **Writing.** The writer keeps the write buffer the host gives. It
  packs a block into the buffer, or beside it in a scratch region, and
  keeps the list of each partition in regions the host gives. A call
  that writes returns the ranges to write: the buffer when it is full,
  and a block larger than the buffer on its own. A finish returns the
  counts of the lists, their entries and the trailer.
- **Reading.** For a finished set's file the host gives the file's
  length. The reader asks first for the trailer at the end of the file,
  then for the counts of the lists, then for the entries of one
  partition, a page at a time, and then for each block whole. It checks
  each part before it asks for the bytes the part names, and asks for no
  byte past the file. The host reads each range and hands the bytes
  back.
- **Seeking.** A reader goes to the block a write placed at a given byte
  by a binary search over the partition's entries, which lie in the
  order of their offsets: it asks for the page of each entry it looks
  at, and keeps no entry but those of its page.

Every set, a serial one too, ends with its lists and a trailer. A
finished set frees its lists and what it packed aside, and reads its
own partitions back from its file a page at a time, as another
participant reads them. Lists kept in memory grow with the input, and
counted against the limit they made a RIGHT join at 64 kB split into
5760 partitions where it splits into 22.

Every number of a block lies in the machine's byte order: a temporary
file is read back by the query that wrote it, on the same machine. The
layouts of a header, of a set's file and of its lists are drawn in the
documentation of `BlockHeader` and of the module `set`, which the
spec's requirements on the format link to.

- **Refused: one file for each partition.** A level of many partitions
  would open as many files, each with a buffer of its own.
- **Refused: a checksum of each block.** It would cost every read, and
  a temporary file has no reader but its own query. Damaged bytes that
  pass every check read as other data; the promise is that a read goes
  wrong in no other way.

## Packing

A chunk of `relhash`'s records is stored packed by lanes, when it packs
and gets shorter, and as it is otherwise. A lane is the same 4 bytes of
every record, and each lane is stored at the width its values need in
this chunk, down to nothing when all are zero. Unpacking gives back the
chunk byte for byte, but for the lane of the next-record references,
which a chunk not yet linked has as zero. The offsets of a record's
fields are `relhash`'s own constants, so the packing checks the format
of `relhash` it knows, and a new one stops the build until the packing
is checked against it. `pack` draws the body.

A chunk of columns keeps rows that are only written and read back,
never linked or probed: a join's outer rows that wait for their
partition, a sort's runs, a grouping's rows. Each word of a row lies in
the lane of its column, so a window of a lane reads as the values of a
column with no gathering. On disk each lane is stored by frame of
reference: its least value, and each value's difference from it in the
narrowest of a few widths that holds the largest difference.
`columns::pack` draws the body. A packed chunk may take a little more
than its rows, for its counts and a descriptor of each lane: the slack
that `columns::pack_slack` gives, which a bound on a block's bytes
leaves room for.

A value by reference, such as a long text, lies in a chunk of values
written just before the chunk of columns that refers to it, the two a
pair of blocks; a row's word for it is a reference, the number of the
chunk of values and the byte of the value in it, drawn in
`columns::value_ref`. Where a value ends is the rule of its type, which
only the host knows: the check of a chunk read back asks the host
(`spill_columns::ValueEnds`) whether each value ends inside its chunk.

Each unpacking checks the whole body before it writes a byte of its
chunk: the counts, a code or a width for every lane, every lane inside
the body, nothing past the last. Then it copies the lanes it checked.
So a body it refuses leaves the chunk as it was, and a proof of the
check need not reason about the copy.

- **Refused: compressing blocks.** A general compressor, lz4, runs at
  1.3 GB/s, where packing by lanes runs at 7 to 10 GB/s, and spilled
  rows are mostly numbers of a few widths.

## The words of a spill and their rules

The words of a spill count, for each partition of a level, the bytes it
holds in memory, its records, and its flags: on disk, joined in a round,
taken by one participant alone. They count the table's bytes in all,
and keep its budget. The same code runs over the words of one process,
plain numbers in cells, and over atomic words in memory that several
processes share; `shared_spill` documents the words and their layout.

Two rules decide by them. The rule that sends a partition to disk,
`Spill::evict`, sends the partition of the most bytes while the memory
passes a share of its limit; near the end of a build it also sends the
partitions still in memory when they hold too few rows to be worth
probing, since each would cost every outer batch a probe. The rule of a
split, `splits`, says whether a partition read back splits into a
level below, by the room its bytes leave and the share of the level's
rows it holds. Each node gives the rules weights, which its settings
set: the rules are one, their values the node's.

- **Refused: a rule of each node.** A join, a grouping and a shared
  build would each keep a loop that sends partitions to disk, with only
  their values apart.

## The plan of a level

A level's plan takes the shape of a node's spill, its least and most
partitions, the room each partition keeps and the bounds of its chunks,
and the level: the bytes it expects, its limit, the bits of the hash the
levels above took. Its partitions start from the least and double while
the level would need more, as far as the shape, the limit and the bits
of the hash allow; its chunks share the limit among them, but hold four
records at least. A level below takes the bits past its own, and what
the levels above leave of the limit, a quarter of it at least.
`plan::plan` documents the rule.

A level takes the low bits of a row's hash, from the first level up,
and the buckets of a `relhash` table take the high bits, so that the
rows of one partition still spread over the buckets of its table while
the level's bits stay below the table's; a deep level of a large table
leaves its partition's rows fewer buckets. A
host that keeps rows in a table whose buckets take the low bits, such
as a dictionary of values, mixes the hashes it gives a level first.

## The machines

A machine is the protocol of one node's spill, as plain words of a
fixed size that the host keeps where it likes, such as in memory it
shares with C. The host steps it: each step takes an event and gives an
action.

- **Events** are what the node saw: a batch went into the table, a
  chunk of a partition is full, the input ended, a block was read, the
  others arrived at a barrier.
- **Actions** are what the node does: give a partition a chunk, write
  a chunk, free one, read the next block, wait, give out rows.

A machine decides by the numbers the node passes in, and never for a
row. Where a decision is the same for every row of a partition, the
machine makes it for the partition, and a kernel applies it to the
rows: an outer row of a join is kept in memory, sent to disk, or
answered at once, as its partition's route says.

- **The join's spill** (`join`). A build in memory; a build that spills
  at level 0; the probe of the partitions kept in memory; then the
  partitions on disk one by one, each split into a level below, joined
  whole, or joined in pieces over several passes of its outer rows; and
  the tails of RIGHT and FULL joins, which give out the inner rows that
  found no match. A piece ends past its room where a group of blocks
  ends, values with the records that refer to them, so that no record
  is read apart from its values.
- **The grouping's spill** (`group::Group`). Groups by partition: a
  partition's records go to disk whole, new records of its groups merge
  with them at the end, and each partition read back merges or, too
  large, splits into a level below. A partial grouping, whose groups may
  go up unfinished, sends a full table up instead, unless that would
  fold too few rows, and then spills.
- **The grouping's rows** (`group::Rows`). For aggregates whose states
  cannot merge: past its share of the limit, once it holds a least
  count of groups, the table freezes, taking no new group, and the rows
  of the groups it lacks go to a level of rows on disk; each partition
  of that level is read back into a table of its own, which may freeze
  in turn, a level further.
- **The sort's runs** (`sort`). When the rows in memory go to disk as
  a run; the sets that hold the runs, a few to a file; how many runs one
  merge reads at once, by what the limit holds of two blocks of each;
  the passes of the merge; and how many runs the last merge reads. A
  rescan reads the last merge again; a sort read backward merges into
  one run, which it reads by blocks. The merge of the rows stays the
  host's, which alone knows their order.
- **A build by several processes** (`shared`). The phases of a shared
  build, from `phases`: append, flush, size, link, write the outer side,
  probe, then free; the split every participant sees at once, once its
  filter is published; a late participant's catch-up; the partitions
  sent to disk while the build goes on; the walk round the partitions on
  disk, each released, given to a round, or taken alone; a participant
  that stops early; and who frees what. A round over a partition has
  phases of its own, `phases::Participant::round_step`: elect, allocate,
  load, probe, free. The barriers stay the host's: a step answers "wait"
  where the node waits.

A step that refuses an event leaves its machine as it was, and words
that are no machine's, as any words a host gives may be, are refused as
a wrong call. Each machine's module documents its events, its actions
and its states.

- **Refused: the decisions as separate calls.** Their order, their
  state and their mistakes would stay in the node, where no model
  reaches them.
- **Refused: one machine for every operator.** Their protocols have
  different states. What they share, the plan, the words of a spill and
  the file of a set, they share as parts.
- **Refused: async Rust.** A future is a state machine the compiler
  writes, hard to keep in shared memory, to bound, or to prove.
- **Refused: the phases of a build that never spills outside the
  crate.** Two machines over one barrier would hand the build over to
  its rounds across the boundary of two crates. The cost: a shared build
  that never spills still steps the machine of a spill.

## How a host puts the machines together

A shared join runs two machines in each participant, and the host
passes partitions between them.

- The **shared build's machine** owns the words in shared memory, the
  phases, the walk round the partitions on disk, and which partition
  goes to whom. It decides when the work is complete, and it cleans up.
- The **join's machine** owns the participant's own level, its pieces
  and its passes. It never frees a shared region.

```
  shared build's machine        host              join's machine
  ----------------------        ----              --------------
  build phases, then the
  walk:  Next  <-------------- the next
         Visit(p) ----------->  partition?
                  <----------   p has outer rows?
         (released, or)
         Alone(p) ----------->            --->  Take(p): split, joined
                                                whole, or in pieces
         Round(p) ----------->  a round's phases,
                                every participant
                                on p together
         Walked   ----------->  done
```

When its join's machine has done with a partition, the host asks the
shared build's machine for the next. A partition one participant takes
alone goes to its join's machine as an event, and is joined as a serial
join joins a partition read back. A partition of a round is joined by
every participant together, through the round's phases. The tail of a
RIGHT or FULL join's rows kept in memory is given out as the build is
left, before the walk. A serial join asks its own machine for its next
partition, with no shared one.

## One owner for each rule

Each of these rules has one owner, the crate, and no node keeps a copy:
the loop that sends partitions to disk; the setting up of a level, its
partitions, chunks and words, the bytes level 0 expects and the shift of
a level below; the rule of a full chunk; the partition of a hash; the
bits each level takes; the appending of rows to a chunk of columns and
its reading back, which a grouping's rows, a sort's runs, a join's outer
rows and the gather of parallel workers share; which partitions to
open, and where outer rows go; and the share of memory at which a
grouping's spill starts. One writer of chunks of columns serves the
sort and the gather. A grouping and a join write their rows by
partition in loops of their own around the crate's append by
partition, since each writer tried in their place was 7 to 28 % slower.

Two rules of a join look alike but are two. A partition read back is
split into a level below by the room its bytes would leave, a
parameter of the machine. A partition joined in pieces takes pieces of
two thirds of the free room, the last third kept for each piece's
index.

## Errors of two kinds

An error of the crate is one of two kinds: a **wrong call**, such as an
event a machine does not expect in its state, a partition past a
level's count, or words that are no machine's; or **damaged bytes**,
read back from a file. The error names the rule it broke, and two
numbers, in one word of 64 bits, as `relhash`'s errors do; a test sees
which rule refused, so it can show that each check is needed. Tessera
reports the first kind as SQLSTATE `XX000` and the second as `XX001`.

A limit of the host, such as a row longer than PostgreSQL allows, is
neither kind: it stays the host's error. A call that needs more memory,
and an action the host could not do, are outcomes of the protocol, not
errors of the crate. A source of chunks or of the values of rows that
the host gives, and that cannot give one, is a wrong call naming the
chunk, or the column and the row; why it could not is the host's, which
keeps its reason and reports it in place of the crate's error.

## The promise over any bytes

Whatever bytes a file holds, a read of the crate returns the data or an
error. It never crashes, never loops without end, and never asks for a
region past the bounds its caller gave.

The caller gives a bound for each kind of block: for a chunk of records
or of columns, by the plan of its level; for a block of values, by the
longest one the set's writers wrote, which the writer measures, so that
a node need not know its largest row before it writes. The participants
of a shared set agree on the largest in shared memory: each raises the
shared bound before the barrier that ends their writes, and all read it
after, before any of them reads another's file. A read asks at most
for one block within the larger of the two bounds, with its header and
the slack of packing, or for one page of a list, and never for more
than the file holds; a block past the bound of its own kind is refused
once its header is read. The promise holds for a file that nobody
writes while it is read.

The promise is shown by six means, each covering what the others
cannot:

1. **Trust points.** Every read of bytes the crate did not write goes
   through one layer and carries a mark, and the list below says for
   each what the bytes decide and what checks them. A script checks the
   marks.
2. **Proofs by Kani,** a model checker that runs a check of the crate
   over bytes that may hold any value and shows, for every value, what
   the check promises: of the checks of a header, a trailer and the
   lists, of the bounds of unpacking records and columns, and of the
   arithmetic of a level's plan and of the rule that sends a partition
   to disk.
3. **The damage matrix.** A file the crate wrote, damaged in one place
   from a closed list, against every read.
4. **Fuzzing,** millions of inputs a program makes up and changes to
   reach new code: any bytes as a file; a set written and damaged where
   the fuzzer chooses; and any events and numbers given to a machine,
   or any words as a machine, a reader or a writer, which must answer
   with an action or a wrong call, never with a panic.
5. **Mutants,** small changes of the crate's code that a tool makes one
   at a time: each must fail some test of the crate's own, or is listed
   with the reason none can, most because it changes nothing a caller
   can see.
6. **Miri,** which runs tests and stops at undefined behavior: over the
   crate's test of its records and over the tests of the C entry points
   of the spill, and over most of the crate's tests on a big-endian
   target, where packing narrows values by their other end.

## Where the crate trusts its bytes

A **trust point** is a place where a call takes a value from bytes it
did not write in the same call, a value it must check before the value
decides anything. Most of these bytes came from a file: a block, the
pages of a file's lists. The rest are a chunk that the host gives to be
packed or appended to, which may hold anything too.

What a value decides says what a wrong one would do:

- an **address**, where the call reads or writes next: a wrong one
  would reach outside the bytes it was given;
- the **length of a loop**, how many steps a walk takes: a wrong one
  would let the call go on and on;
- only **data**, such as a key or a value of a row: a wrong one gives a
  wrong answer, which no check can see, but no crash.

Every such read goes through one layer, the module `read` of the crate,
so the reads are few and can be listed. Each call of the layer carries
a comment `// trust:` with the name of its point below.
`.github/scripts/check-trust.sh` fails when a call has no mark, when a
mark names no point of this list, when a point is marked nowhere, when
the layer has a function the script does not know, when a word is made
of bytes outside the layer, and when a module imports the layer's
functions, so that a call could go unmarked. A byte taken from a slice
by its index is no word, and the script cannot tell a byte of a file
from another: the review of a change to a module that reads a file
looks for one. A point names, when they exist, the proof and the rows
of the damage matrix that cover it.

A proof under a point is one of `crates/relspill/src/proofs.rs` or of
the module `proofs` of the module it checks, which `cargo kani -p
relspill` runs. Kani, a model checker, runs the crate's own check over
bytes that may hold any value, and shows for each value that what the
check passes keeps the reads after it inside their bytes and that
nothing panics or overflows.

Two families of proofs cover no trust point, as their numbers come from
the host, not from a file. The plan of a level, for any shape and any
level: `a_level_plans_partitions_of_its_shape`,
`a_plan_sizes_the_partitions_given` and
`a_level_below_takes_what_is_left_a_quarter_at_least`. The rule that
doubles the partitions weighs the bytes a level expects, a float, by
their count; the solver Kani gives its formulas to, which reasons over
bits, takes that product for a count of any power of two in more than
five minutes, and for counts it knows in
seconds. So the rule is proved whole for the shapes that start from one
partition, which try every count, and from four, as the joins and
groupings of the nodes do:
`a_level_from_one_partition_plans_the_first_that_does_not_double` and
`a_level_from_four_partitions_plans_the_first_that_does_not_double`.
The rule that sends a partition to disk, over the words of four
partitions that hold any values and any memory, is the one its
documentation gives, computed in the proof beside it:
`a_grouping_sends_what_the_rule_says` and
`a_join_sends_what_the_rule_says`, with the weights the nodes' settings
give by default, the join's with no share of the rows in memory too, as
a shared join's takes them. The solver takes the rule with weights of
any value in half an hour, past what a proof may take in CI, so the
weights are the nodes'.

The rows of the damage matrix under a point are damages of
`crates/relspill/src/set/tests/damage.rs`. Its test
`every_damage_against_every_read` writes a set of four partitions:
blocks of values of four lengths; chunks of columns, one with its
values, one without, one whose every lane packs to a constant and one
of lanes of 2 and 4 bytes; chunks of records that pack, by lanes of
every width, and one stored as it is; and a partition of no block. It
damages the file in one place, at and around the limits of a field,
and runs every read of the crate over it twice: each partition opened
and read to its end; its chunks of columns checked as a pair, checked
with their values by a host that knows where a value ends, and read by
windows, each of which must give the chunk's lanes; then a seek to
each of its blocks, which must give the block the read gave there.
After a read that fails, the body is asked for again and three blocks
more. A read must end, with its data or the error of the damage's
rule, and ask for no byte past the file or past the region it names;
a damage that is more than data must be seen by some read; and the
whole matrix has a deadline, so that a call that does not end fails
it. Every rule of damaged bytes that the sources of the reads name is
some row's, but six that no damage of a written file reaches through
the reader: the test lists each with the check that refuses the same
bytes first, and fails on an error of damaged bytes in a form its scan
of the sources cannot read. Checks that an earlier check on the
reader's path covers, such as the counts of a packed body that the
reader compares with its header first, are reached alone by the tests
of each call, the proofs and the fuzz targets. Two checks guard a file
that changes between the calls of a read, which the matrix's never
does: an entry's end against the lists in `Reader::check_entry`, and a
block's entry against the largest in `Reader::next`. A node reads its
files through the same calls, by the C entry points that map to them,
and `test/sql/spill_damage.sql` damages the blocks a join, a sort and
a grouping read back, through injection points of PostgreSQL.

The fuzz targets of `crates/relspill/fuzz` give the reads and the
machines what no list can: `bytes` reads any bytes as a set's file, of
a set the bytes before them choose, and as a block and the packed
bodies a block holds; `damaged` writes a set from the fuzzer's bytes,
damages it in one to eight places they choose, and reads every
partition back, each block, each chunk of columns checked and read by
windows, and seeks; `machines` makes a machine of the crate from any
params or from any words, a set's reader and writer among them, and
steps it by any events and numbers over host regions of any numbers,
which grow as its actions ask. A target fails on a panic, a call that
does not end, a read a step asks for past the file or its region, an
error of another kind than its call promises, and a machine a refused
step changed. A host makes regions for at most as many partitions as a
spill's words count, the most a set takes; the words of a writer of
more are refused before a call walks its lists.

The words of a spill that the participants share, and the block lists
a host keeps for a writer, are the host's memory too, but they hold no
bytes of a file and are no trust point: the crate checks each where it
reads it, as the documentation of the words and of the writer says.

- **`header`**: the fields of a block's header, read back.
  - Decides an address and the length of a loop: the bytes of the body
    to read, to unpack and to copy, by its kind and its packed length.
  - Check: `BlockHeader::read`, which compares the magic, the version,
    the kind and the fingerprint, then every length its kind allows,
    within the bound of its kind, and the level.
  - Error: damaged, from `Magic` to `Level`.
  - Proof: `a_header_that_passes_has_the_lengths_of_its_kind`, and
    `a_header_written_reads_back_as_it_was` for the writer, which
    refuses what the reader would.
  - Matrix: the rows from `magic` to `columns packed into no 8s`,
    among them a pair's block of another kind or number.
- **`trailer`**: the four words at the end of a set's file.
  - Decides an address: where the lists start, and so where every read
    of the lists goes; and the length of a loop, the entries the lists
    can hold.
  - Check: `Reader::read_trailer`: the magic, the count of partitions,
    the start of the lists within the file, the fingerprint, and lists
    that fill whole entries after their counts.
  - Error: damaged, `Trailer`, `TrailerMagic`, `TrailerPartitions`,
    `TrailerLists`, `FileFingerprint` or `ListsRoom`.
  - Proof: `a_trailer_that_passes_puts_the_lists_before_it`.
  - Matrix: the rows from `a file shorter than a trailer` to `file
    fingerprint`.
- **`list-counts`**: the count of blocks of each partition, the first
  part of the lists.
  - Decides an address and the length of a loop: where the partition's
    entries start, and how many the reader reads.
  - Check: `Reader::read_counts`: no count past the entries the lists
    hold, and all of them together exactly those entries.
  - Error: damaged, `ListCounts`.
  - Proof: `counts_that_pass_keep_the_partition_s_entries_in_the_lists`,
    a step of a page from any counts that keep to the lists; and
    `counts_taken_open_the_partition_s_entries_inside_the_lists`, the
    open once every count is taken.
  - Matrix: the rows from `a count one more` to `a block counted in
    the next partition`, and `lists an entry later` and `lists an
    entry earlier`, which move the counts.
- **`entry`**: an entry of a partition's list, the place and the bytes
  of one block.
  - Decides an address and a length: the bytes of the file a block read
    asks for, and the length of the block buffer.
  - Check: `Reader::check_entry`: a length of a header at least and of
    the largest block at most, and a block that lies whole before the
    next block of its partition and before the lists. A page read again
    is checked again: each entry before the next of the page, and all
    of them before the lists.
  - Error: damaged, `EntryLength` or `EntryPlace`.
  - Proof: `an_entry_that_passes_names_a_block_before_the_next`;
    `entries_that_open_a_partition_name_blocks_one_after_another`, as
    the open passes them; and
    `a_page_that_passes_names_blocks_one_after_another`, as a page is
    read again.
  - Matrix: the rows from `an entry a word later` to `two entries
    swapped`.
- **`packed-counts`**: the two counts at the start of a packed body,
  read when its block is read.
  - Decides nothing but a comparison: the length the counts give must
    be the header's, before the host makes a buffer by it.
  - Check: `Reader::read_block`.
  - Error: damaged, `PackedHeader`.
  - Matrix: `records one more`, `records a word longer`, `rows one
    more`, `words one more`, `columns a word longer` and `packed
    records read as columns`.
- **`packed-records`**: the counts, the lane codes and the lanes of a
  packed chunk of records.
  - Decides addresses and the length of loops: the records and the
    lanes to fill, and where each lane's values lie.
  - Check: `unpack`, before it writes a byte of the chunk: counts that
    fill the chunk exactly, then a code for every lane, every lane
    within the body, the lane of the next-record references empty,
    every record of the body's length, and no byte past the last lane.
    Then it copies the lanes it checked.
  - Error: damaged, from `PackedCounts` to `PackedTail`, the chunk as it
    was.
  - Proof: `a_body_too_short_for_its_counts_is_refused`;
    `counts_that_pass_fill_the_chunk`, for any counts, any length of the
    body and of the chunk; and
    `lanes_that_pass_lie_inside_the_body_and_fill_the_chunk`, for any
    codes and lanes of two records of 24 bytes, and the copy after them
    over any bytes the chunk held. That the data lanes come back as they
    were packed is the tests'.
  - Matrix: the rows from `records of 5 bytes` to `a byte of records
    stored as they are`.
- **`packed-columns`**: the counts, the descriptors and the lanes of a
  packed chunk of columns.
  - Decides addresses and the length of loops: the rows and the lanes
    to fill, and where each lane's values lie; a lane's least value and
    its values are data.
  - Check: `columns::unpack`, before it writes a byte of the chunk:
    counts within the limits of a chunk that fill the chunk exactly,
    then widths that exist, every lane within the body, and no byte
    past the last lane. Then it copies the lanes it checked.
  - Error: damaged, `ColumnsCounts`, `ColumnsLimits`, `ColumnsFill`,
    `Descriptors`, `LaneWidth`, `ColumnsLaneEnd` or `ColumnsTail`, the
    chunk as it was.
  - Proof: `a_body_too_short_for_its_counts_is_refused` of the module
    `columns`; its `counts_that_pass_fill_the_chunk`, for any counts,
    any length of the body and of the chunk, within the rows a block's
    packed length can name; and its
    `lanes_that_pass_lie_inside_the_body_and_fill_the_chunk`, for any
    descriptors and lanes of two rows of one word, and the copy after
    them over any bytes the chunk held. That the lanes' values come back
    as they were packed is the tests'.
  - Matrix: the rows from `more words than a chunk holds` to `the
    zeros of a descriptor`.
- **`columns-header`**: the header of a chunk of columns, the host's or
  one read back.
  - Decides addresses and the length of loops: where each lane lies and
    how many rows a call reads or writes.
  - Check: `columns::shape`: the magic, and words, rows and a capacity
    within the limits of a chunk and the bytes it holds. A chunk read
    back is checked by `spill_columns::check` through it.
  - Error: a wrong call, `ColumnsChunk`, `ColumnsMagic` or
    `ColumnsShape`, damaged for a chunk read back.
  - Matrix: none: a chunk read back takes its header from its packed
    counts, which `packed-columns` covers.
- **`column-references`**: the NULL bits and the words of a column by
  reference, in a chunk of columns read back.
  - Decides an address: the place in the chunk of values that a word
    names.
  - Check: `spill_columns::check`, once for the chunk: the words and the
    rows the caller expects, and each word of a row not NULL a
    reference into the values, at a byte aligned to 8 or at their end,
    to a value that ends inside them.
  - Error: damaged, `ReadWords`, `ReadRows`, `ValueRef` or `ValueEnd`.
  - Matrix: the rows from `references a byte later` to `a constant
    chunk of no words, its rows and a lane to match`.
- **`column-words`**: the words and the NULL bits of a checked chunk of
  columns, read by windows.
  - Decides only data: a word of a row, which a reference the check
    passed already is.
  - Check: none past `spill_columns::check`; `window` checks the
    chunk's shape again and its caller's bounds, not the words.
  - Matrix: `a least word of a column by value` and `a constant of
    NULL bits`, both data.
- **`host-chunk`**: a chunk the host gives to be packed, or appended
  to: its used mark, its records' lengths, its lanes.
  - Decides the length of loops when it is packed: the records and the
    lanes; the rest is data.
  - Check: packing takes only a chunk whose used mark covers it and
    whose records have one length, and stores any other as it is;
    `columns::shape` checks a chunk of columns first.
  - Error: none for records, which are then not packed; a wrong call for
    columns.
  - Matrix: none: the bytes are no file's.

## No unsafe code, no allocation

The crate's library forbids unsafe code in every build, its unit tests
too. The words that several processes share come to it as a slice of
atomic words that its host makes, so the unsafe code that maps them
lies in the host's entry points. The unit tests check, around each call
they make, that it allocated nothing: an allocator that counts each
thread's allocations, a crate of its own because an allocator is unsafe
code, is linked into them under a setting of the build, as loom, a
checker of the orders in which threads may run, is for its models. One
test from outside the crate, `crates/relspill/tests/alloc.rs`, a program
of its own, keeps a counting allocator of its own; another,
`crates/relspill/tests/records.rs`, makes a table through `relhash`'s
unsafe calls, which is why Miri runs it.

## The forecast, probed

The contract, [contract.md](contract.md), names the changes the crate
is likely to see and what each would touch. A probe makes such a
change on a branch that is never merged and counts the parts and the
interfaces it touched. Where a probe stops short of the whole change,
what it did not make is counted by reading the code it would reach.

- **A merge join over sorted runs.** No module of the crate. A model
  host sorts a merge join's two inputs through a machine of the runs
  each, both spilling, holds each sorted output whole, and joins them
  by merging: the machine takes its second user unchanged, with no
  format and no other machine, as the forecast says. The probe makes
  no node, and reads no output as a stream. PostgreSQL's merge join
  already reads the output of Tessera's sort, with a Material above it
  for mark and restore. A merge join of Tessera's own that steps the
  runs itself meets the host of the runs where the sort node keeps it:
  28 of the 31 functions of the sort's external part depend on the
  sort's state. The forecast named the node that steps the machine;
  read as a merge join's own node, that node cannot take the host of
  the runs as it is, which is past the forecast.
- **A host with another manager of temporary files.** No module of the
  crate, as the forecast says. The probe gives the host's set of files
  a name of its own and keeps PostgreSQL's manager under it: it shows
  where the set's type reaches, not another manager's calls. The set,
  PostgreSQL's `SharedFileSet`, is a type of the interface of the
  spill's host, `runtime_spill.h`, in its config and three calls, and
  the join's node keeps one by value in its shared memory and lays it
  out there in four calls. Past the forecast, which named the host's
  file calls alone.
- **A new kind of block**, a chunk of the states of aggregates stored
  as it is. In the crate: the kind, its code and its bound; the
  header's proof of the lengths of each kind; and one row of the damage
  matrix. The writer, the reader and the packing take it unchanged.
  One interface past the forecast: the C API's codes of kinds, in two
  maps of codes and the header's enum. The enum of kinds was not marked
  as one that grows, so a caller outside the crate that matched on kinds
  changed with it; it is marked now, and such a caller keeps an arm for
  kinds to come. The machine that writes states and the host's
  code are not made; the nodes name a kind of block in 19 places of 3
  files, and the runtime in 3.
- **A change of the record of relhash**, its size and its NULL bits
  swapped. In the crate, the packing alone: the check of the format it
  knows, which stops the build as it should, and two of its tests,
  which pin the lane of the length. Its proofs and the damage matrix,
  which the forecast named, the machines and the spill's host take the
  layout unchanged. Beside the spill, the table's own interface changes
  with its record, as it does for any change of it: the C mirror of the
  record's layout and version in the table's header, which the kernels'
  module checks against relhash when it loads, two tests of the C API
  of the table, and relhash's tests of its format, which the probe
  leaves failing.
- **A grouping by several processes that spills.** No module of the
  crate. A model participant steps the build's machine with the
  answers a grouping would give: nothing for the outer side, of which
  it has none, its groups at the probe, and, keeping every inner row as
  a grouping keeps every group, each partition on disk that holds
  records taken in the walk round them. It answers the actions without
  their work: it holds no rows and no groups, runs alone, and joins no
  round of several participants. So it shows that the machine's steps
  and parameters serve a grouping as they are, not the grouping's work;
  the grouping's machine for a partition taken alone and the node are
  not made. The names of the machine's actions and events are a
  join's, which a grouping's host answers in its own terms.

## What we decided not to do

- **File input or output in the crate.** See "The crate and its host".
- **Reading ahead.** A read asks for one block. The host may read
  ahead; the crate does not ask it to.
- **Compressing blocks, and checksums of blocks.** See "Packing" and
  "The file of a set".
- **Retrying an action that failed.** See "The crate and its host".
- **Work for a participant that comes after the build.** One that
  attaches to a round in progress takes part in it. One that comes
  after the part of the build in memory is freed finds no work, though
  rounds still run: work for it would need a path that only a
  participant late by just that much takes, and no test reaches.
- **Values by reference in the crate.** Their size, where one ends, and
  the flattening of a value expanded in memory belong to the host; the
  crate copies the bytes its host gives.
- **Spilling the states of sums instead of rows**, and **a spill for a
  merge join**: neither is made; each would be a machine of its own.
- **Publishing on crates.io.** `publish = true` marks the crate's public
  API, the surface its rules and checks keep; the crate is not
  published.
- **32-bit targets**, as for `relhash`.

## Files

- **`crates/relspill/Cargo.toml`**, **`crates/relspill/README.md`**:
  the crate, its features, and its front page with an example.
- **`crates/relspill/src/lib.rs`**: the crate's root, the header of a
  block and the bounds of each kind.
- **`crates/relspill/src/error.rs`**: the errors, their two kinds and
  their rules.
- **`crates/relspill/src/read.rs`**: the one layer of reads of bytes
  the crate did not write.
- **`crates/relspill/src/pack.rs`**,
  **`crates/relspill/src/columns.rs`**: the packing of records and the
  chunks of columns, packed and not.
- **`crates/relspill/src/spill_columns.rs`**: appending rows to chunks
  of columns, by partition, and the checks of a chunk read back.
- **`crates/relspill/src/set.rs`**,
  **`crates/relspill/src/set/reader.rs`**: the writer and the reader of
  a set's file.
- **`crates/relspill/src/plan.rs`**: the plan of a level.
- **`crates/relspill/src/shared_spill.rs`**: the words of a spill, the
  rule that sends to disk and the rule of a split.
- **`crates/relspill/src/phases.rs`**: the phases, the rounds and the
  counters of a shared build, which its machine steps.
- **`crates/relspill/src/join.rs`**,
  **`crates/relspill/src/group.rs`**, **`crates/relspill/src/sort.rs`**,
  **`crates/relspill/src/shared.rs`**: the machines.
- **`crates/relspill/src/testing.rs`**: what the tests share, the
  check that a call allocates nothing among it.
- **`crates/relspill/src/loom.rs`**: the models of loom.
- **`crates/relspill/src/proofs.rs`**,
  **`crates/relspill/src/pack/proofs.rs`**,
  **`crates/relspill/src/columns/proofs.rs`**,
  **`crates/relspill/src/set/reader/proofs.rs`**,
  **`crates/relspill/src/plan/proofs.rs`**,
  **`crates/relspill/src/shared_spill/proofs.rs`**: the proofs by Kani,
  built only for Kani.
- **`crates/relspill/fuzz/`**: the fuzz targets, a crate out of the
  workspace.
- **`crates/alloc-count/`**: the allocator that counts each thread's
  allocations, for the tests alone.

## Tests

- The tests of each module, beside it: its calls, its refusals and its
  edges. Property tests run the join's, the grouping's and the sort's
  machines over random rows, limits and failures, with files as vectors
  and memory as a count, against what an operator without a spill gives
  (the join's model in `crates/relspill/src/join/model.rs`). The shared
  build's machine has tests of its steps, and loom, a checker of the
  orders in which threads may run, runs its phases, its rounds and its
  words with several participants (`crates/relspill/src/loom.rs`).
- Hand-written cases beside the property tests reach what a random test
  seldom does, each checking that its path ran: levels below and the
  bits they take, a partition too large to split joined in pieces and
  where each piece ends, the end of a build, a partial grouping, and a
  step out of turn, which changes nothing.
- `crates/relspill/src/set/tests/damage.rs`: the damage matrix.
- `crates/tessera-capi/benches/spill.rs`: a program of `tessera-bench`
  that counts the instructions and cycles of packing and unpacking
  records and columns, of appending rows by partition and of a header,
  the measure of the kernels' cost beside their machine code.
- `crates/relspill/tests/records.rs`, `crates/relspill/tests/alloc.rs`:
  the crate from outside, its records packed and read back, and no call
  that allocates.
- `cargo test -p relspill`, also with `--cfg count_allocations`, which
  counts every call's allocations, and with `--cfg loom`, the models;
  `cargo kani -p relspill`, the proofs; `cargo fuzz` in
  `crates/relspill/fuzz`, the fuzz targets; `cargo mutants -p
  relspill`, the mutants; and Miri, as CI runs it.
- `.github/scripts/check-trust.sh`, which checks the marks of the trust
  points against the list above, and its own cases in
  `.github/scripts/test-check-trust.sh`.
