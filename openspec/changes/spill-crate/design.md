## Context

This design is about the move. Its folder is deleted when the change
closes. What a user of the crate needs is then written into the design
of `relspill`; this context does not go there.

### What a spill is

A hash join builds a hash table from one input, the inner rows, and
looks up each row of the other input, the outer rows, in it. A grouping
keeps one entry for each group in a hash table. When the table grows
past its memory limit, the operator spills:

1. The hash of each row picks one of the level's partitions.
2. Some partitions stay in memory. The others are written to a
   temporary file, a block at a time. Rows that arrive later for a
   partition on disk go to disk too; in a join, so do the outer rows of
   such a partition.
3. When the input ends, the partitions in memory are finished first.
   Then each partition on disk is read back and handled alone.
4. A partition read back that still does not fit is split into a level
   below, by other bits of the hash. When it cannot be split, because
   its hash has no bits left or its keys are all equal, a join reads it
   in pieces: it loads one piece of the inner rows, reads all the outer
   rows of the partition against it (one pass), and goes on to the next
   piece.

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

A sort spills another way. It sorts what fits in memory and writes it
out as one sorted run. At the end it merges the runs, in several passes
if there are many.

In a parallel query, several processes can build one hash table in
shared memory. Each of them is a participant. They go through the
phases of the build together, and wait for each other at barriers
between the phases. After the build, the partitions on disk are handled
in rounds: in a round, all participants work on one partition together,
or one participant takes a partition alone.

### Words used here

- **Block.** What one write puts on disk: a header and a packed chunk.
  A block holds records (the rows of a `relhash` table), columns (rows
  stored column by column), or values (the bytes of values by
  reference).
- **Chunk.** A group of rows in memory. `relhash` keeps a table as
  chunks of records; a sort or a grouping keeps rows as chunks of
  columns.
- **Level.** One split of the rows into partitions. Level 0 is the
  first; a partition split again makes a level below. The level plan
  says how many partitions a level has, how long its chunks are, and
  which bits of the hash it takes.
- **Set.** The temporary file of a level: the blocks of all its
  partitions, in the order they were written, and a block list for
  each partition. A shared set is one such file for each participant,
  which the others read. The end of such a file, the trailer, says
  where its block lists are.
- **Spill words.** The numbers a spill keeps for each partition (its
  bytes, its records, its flags) and its memory budget. When several
  processes share a build, these words live in shared memory.
- **Host.** The program that uses the crate. Here it is Tessera inside
  PostgreSQL. It reads and writes the files, gives memory, waits at
  barriers and reports errors.
- **Value by reference.** A PostgreSQL value kept behind a pointer,
  such as a long text. An expanded value is a special kind: a pointer
  to a structure in memory, such as an array being built. It must be
  flattened into plain bytes before it is written anywhere.
- **State machine.** A value that remembers where a protocol is. The
  host gives it an event, and it returns the next action.
- **Wrong call, damaged bytes.** The two kinds of errors of the crate.
  Tessera reports them as SQLSTATE `XX000` and `XX001`.

The tools that check the crate:

- **Property test:** random inputs, checked against a rule that must
  hold for all of them.
- **Kani:** proves a property of Rust code for every possible input.
- **loom:** runs concurrent Rust code in every order its threads can
  take.
- **Miri:** runs Rust tests and stops at undefined behavior.
- **Fuzzing:** feeds random bytes, millions of times, looking for a
  crash.
- **Mutation testing** (`cargo-mutants`): makes small changes to the
  code; a change that no test notices shows a missing test.
- **Outside review:** an AI model of another maker reads the code and
  lists the faults it finds.

### The code today

A spill has four kinds of code today.

- **The format, in Rust.** `crates/tessera-spill` (1,719 lines) writes
  and checks the header of a block. It packs a chunk of `relhash`
  records in lanes of 4 bytes, packs a chunk of columns by the span of
  each lane, and plans a level: how many partitions, how long a chunk.
  It depends on `anyhow` alone and has no unsafe code. Its packing of
  records copies `relhash`'s record layout as bare numbers.
- **The spill words and the phases, in the kernels.**
  `tessera-kernels/src/shared_spill.rs` (1,055 lines) keeps the spill
  words and the rule that picks the partition to send to disk. The same
  code runs over atomic words in shared memory and over plain words in
  one process. `phases.rs` (842) moves a participant of a shared build
  through its phases and rounds. `loom.rs` (635) models both.
  `spill_columns.rs` (606) appends rows to chunks of columns.
- **The entry points, in `tessera-capi`.** `c/spill.rs` (670) and
  `c/shared_spill.rs` (522) give C the calls above. `c/spill.rs` also
  reads PostgreSQL's values by reference, with unsafe code that no test
  runs under Miri. Miri runs the tests `columns` and `table`, which
  append, pack and unpack chunks of columns and step the phases, but
  not the tests `spill`.
- **The protocol and the files, in C.** `runtime/spill.c` (945) calls
  PostgreSQL's file functions, keeps the write buffer and the block
  lists, and writes and checks the trailer of a shared file. The nodes
  decide everything else: `hashjoin_spill.c` (2,344),
  `hashjoin_shared.c` (1,698), `agg_spill.c` (1,371) and
  `sort_external.c` (878).

## Goals / Non-Goals

**Goals:**

- The spill as a crate that a database engine other than Tessera can
  use together with `relhash`. Its host gives it memory and does its
  file input and output.
- Every rule of a spill written once, in the crate.
- The faults of today's spill found and fixed before it moves.
- The promise "any bytes, a bounded read" for a file, shown by proofs
  and tests that cover every place where the crate trusts bytes it
  reads.
- The machines shown correct against a model of the operator without
  a spill, and the machine of several processes checked under loom.
- Not one instruction more in a hot loop of Tessera, and a counter
  benchmark for the kernels of a spill.

**Non-Goals:** see "What we do not do".

## Settled choices

These choices are fixed; the rest of the design builds on them.

- The crate is named `relspill`, as a pair to `relhash`.
- It holds the whole file format, block lists and trailer included,
  and does no input or output: it says which bytes to write where, and
  which to read.
- The protocol of levels and pieces, the reading back of partitions,
  and the rounds of a shared build become state machines in the crate.
- The crate depends on `relhash` for the record layout, so that a
  change of the layout breaks the build instead of the data. `relhash`
  makes the layout a small public API, in an early task.
- As for `relhash`: 64-bit targets only; documentation complete by
  itself; `publish = true`, as a mark of the public surface; nothing
  published on crates.io in this change.
- The join's two thresholds stay two. The setting
  `tessera.join_spill_split_room` decides whether a partition read back
  is split; two thirds of the free room decide how large a piece is.
  They are two rules, not one rule copied wrong.
- An action of the host that fails ends the spill. The crate does not
  retry it, and what is left can only be freed or reset.
- The faults that reviews find in today's code are fixed first, as
  tasks of the validation.
- The specs of `hash-table` and `spill-format` change in the task that
  changes a call they promise, not later in a change of their own.

## Decisions

### What goes into the crate

```
              relspill                          Tessera
  +--------------------------------+  +--------------------------------+
  | format: header, records,       |  | values by reference: sizes,    |
  |   columns, references to       |  |   copies, expanded objects     |
  |   values                       |  | files: fd.c, SharedFileSet,    |
  | file of a set: buffer, lists,  |  |   temp_file_limit, tablespaces |
  |   trailer, reads checked       |  | memory: contexts, DSA, the     |
  | level plan                     |  |   measure of what is allocated |
  | spill words, the rule of       |  | barriers, interrupts, errors   |
  |   eviction, the split          |  |   and their SQLSTATE           |
  | machines: join, grouping,      |  | settings, EXPLAIN, the planner |
  |   sort, a shared build and     |  | the nodes, which step the      |
  |   its rounds                   |  |   machines and do the actions  |
  | appending rows to columns      |  | keys, hashes, aggregate states |
  +---------------+----------------+  +--------------------------------+
                  |
                  v
               relhash: chunks, records, partitions, the Bloom filter
```

The test for the line between them: a rule that a database engine
without PostgreSQL would need in order to spill with `relhash` goes
into the crate. A rule of SQL, of PostgreSQL's types or of its executor
stays in Tessera.

- **In the crate:** all of `tessera-spill`; `shared_spill.rs`,
  `phases.rs` and their loom models; `spill_columns.rs`, whose traits
  already take rows and values from a source; the bookkeeping of
  `runtime/spill.c`; and every decision of the nodes listed under "One
  owner for each rule".
- **In Tessera:** `DatumRows` of `c/spill.rs`, the source of rows over
  PostgreSQL's values; the copying of values by reference in the nodes,
  which flattens an expanded value before any writer sees its bytes;
  the calls of `fd.c` and `SharedFileSet`; the measure of memory by
  memory contexts; barriers, DSA, interrupts and errors; and the
  settings, which become the parameters of a machine.

### No file input or output

The crate never opens, reads or writes a file. Its host does, and tells
the crate what happened. This form is called "sans-IO": a protocol is a
value that takes bytes and events, and gives back bytes and requests.

```
   node (C)                                 relspill
   --------                                 --------
   batch of rows appended    -- event -->   machine.step
                             <-- action --  "write chunk 7 of
   tess_spill_write                          partition 3 at 81920"
   (FileWrite at 81920)
   written                   -- event -->   machine.step
                             <-- action --  "give out the rows"
   ...
```

- **Why.** A call of PostgreSQL may raise an ERROR. An ERROR jumps
  straight to its handler, over every function call in between. If the
  crate called the file functions through callbacks, that jump would
  cross the crate's frames, which Rust does not allow for frames that
  own anything; every callback would need a `PG_TRY` around it. With
  sans-IO, every call of PostgreSQL stays in C. The crate can also be
  tested whole: in a test a file is a vector, a fuzzer feeds it any
  bytes, a model runs it in memory, and Kani reasons about it without a
  file system.
- **Refused: callbacks for input and output.** Simpler for the crate,
  but it brings the jump above, and the host's file calls would have to
  be reentrant.
- **Refused: leaving the files in C.** The trailer and the block lists
  would stay a format that only C writes and checks, outside every
  proof and fuzzer.

### When an action fails

An action is outstanding from the step that returns it until the host
reports it done. While a write is outstanding, the host holds the bytes
it names in the crate's buffer, so the crate accepts no call that would
change those bytes. A region is replaced only when no action holds it.

An action can fail: a write that wrote only part of its bytes, a read
that got fewer bytes than it asked for, an error of the file manager,
the limit of temporary files, a cancel. The crate does not retry and
does not try to recover. The host reports the failure. After that, the
set or the machine accepts only a free or a reset; any other call is a
wrong call.

In a shared build, a participant whose action failed never arrives at
a barrier as done, and its claims in the shared words are not taken
back. The host ends the whole parallel query, as PostgreSQL does when
an ERROR in one process ends the others. A host that is not PostgreSQL
must do the same.

Three cases look alike and must be kept apart:

- The file ends before a block ends: damaged bytes, an error of the
  crate.
- A participant wrote no file: its set is empty. The host may say so
  only after that participant has finished; asked earlier, it is a
  wrong call.
- The host could not do a read or a write: the host's own error, which
  ends the spill as above.

The host gives what only it knows: the length of a participant's file,
from which the crate finds the trailer, and which file belongs to whom.
It keeps three ways of ending a set apart:

- releasing a set closes its file and keeps it for the others;
- freeing a set deletes its file;
- resetting a shared set for a new execution deletes every file of the
  set. It comes only after every reader has let go, and it may reuse
  the names of the last execution's files.

The host's barrier orders memory: every write a participant made before
it arrives at the barrier is seen by every participant after they leave
it. The spill words use relaxed atomics where a barrier orders them.
The filter of a shared build is published under a lock before the split
is agreed. The machine's documentation says so, for a host that brings
its own barrier.

The tests cut the host's actions with failures: between two writes,
before a set is published, and during a reset. They check that every
region is freed and every file is gone. They do not check that the
query's output is whole, since the query has failed.

### The file of a set

A set is one file that holds the blocks of every partition of a level,
in the order they were written. A shared set is one such file for each
participant, read by the others. The crate takes over everything that
`runtime/spill.c` does besides calling PostgreSQL:

- **Writing.** The crate keeps the write buffer the host gives. It
  packs a block into the buffer, or beside it, and keeps the block list
  of each partition. A call that writes returns the ranges to write:
  the buffer when it is full, and a block larger than the buffer on its
  own. At the end of a shared set it returns the block lists and the
  trailer.
- **Reading.** For its own set, the crate gives the position and the
  length of each block from its lists. For another participant's file,
  the host gives the length of the file. The crate asks first for the
  trailer at the end of the file, then for the counts, then for the
  entries of one partition, a page at a time. It checks each part
  before it asks for the next. These are the checks C makes today, now
  in one place with the checks of a block's header. The host reads each
  range and hands the bytes back.
- **What C keeps.** `runtime/spill.c` keeps its calls of `fd.c` and
  `SharedFileSet`, the limits PostgreSQL puts on temporary files, the
  names of the files, and the errors those calls raise. Its C API,
  `include/tessera/runtime_spill.h`, keeps its calls, so the nodes use
  a set the same way after the move.

The format on disk does not change in that move: a file written before
it can be read after it.

### Memory the caller gives

As in `relhash`, a call of the crate allocates nothing. Every region it
uses comes from the caller: the write buffer, the block lists, a
reader's buffer, the spill words and a machine's state. A call that
cannot start without a larger region returns how much it needs and
changes nothing; the caller then gives a larger region or fails.

A call that appends rows to a chunk of columns may make progress
first. It appends the rows that fit, says which, and returns what the
next row needs, so that a sort or a gather can write out the chunk and
go on.

So the memory of a spill is allocated where PostgreSQL counts memory,
and a machine can ask its host for the measure it decides by. This also
brings to light the memory that nobody counts today:

- the block lists, which grow with the input;
- the marks of the outer rows that matched, in a join read in pieces,
  which grow with its outer rows;
- the block a join reads ahead past a piece.

Counting them changes when a join sends partitions to disk; an earlier
attempt changed five plans of the suite `join`. So the task that moves
the lists keeps them out of the measure, as today, and a later task of
its own counts them. It counts each region by its capacity, and only
once, since the grouping already counts the memory contexts of its
spill whole. It also sets a rule for what may go past the limit, and by
how much, and for what a node does when memory runs out. A fixed
reserve cannot hold memory that grows with the input. That task names
the plans and the lines of EXPLAIN that change, and edits the specs
that show them.

### The machines

A machine is the protocol of one node's spill, written as a state
machine in the crate. The node holds it and steps it: each step takes
an event and returns an action.

- **Events** are what the node saw: a batch went into the table, a
  chunk of a partition is full, the input ended, a block was read, the
  others arrived at a barrier.
- **Actions** are what the node must do: give a partition a chunk,
  write a chunk, free one, read the next block, wait, give out rows.

A machine decides by numbers the node passes in: bytes, rows, the
measure of memory, the settings.

A machine does nothing for each row. Appending rows by partition,
probing, copying values and writing references to them stay in
`relhash`, the kernels and the nodes. Where a decision is made for each
row today, a machine makes it for each partition, and a kernel applies
it to the rows. For example, where an outer row of a join goes is a
choice made for its partition: kept in memory, sent to disk, or
answered at once because the partition has no inner rows.

The machines, each with the states the code has today:

- **The join's spill.** A build in memory; a build that spills at level
  0; the probe of the partitions kept in memory; then the partitions on
  disk one by one, each split into a level below, joined whole, or
  joined in pieces over several passes of its outer rows; and, between
  them, the tails of RIGHT and FULL joins, which give out the inner
  rows that found no match. The machine takes over the decisions the
  node makes in C today: when to start, the memory limit of a level
  below, the shift of its hash, when a chunk is full, where a partition
  goes, which partitions to open, whether to read in pieces, where a
  piece ends, and the passes over the outer rows.
- **The grouping's spill.** Two mechanisms. Groups are spilled by
  partition; the partitions are given out in two passes, and each is
  split into a level below or merged. The rows of generic aggregates,
  whose states cannot be merged, are frozen on disk and replayed level
  by level. The two take the bits of the hash by different rules, and
  the move keeps both. The machine composes the two: they share the
  file of a set and the writer of columns, not their states.
- **The sort's runs.** More a plan than a machine of many states: when
  a run goes to disk, the sets of runs, the blocks, how many runs one
  merge reads at once, the passes of the merge, and how long the runs
  live. A rescan reads the last merge again, and a scan backward makes
  one final run. The merge of the items stays where it is: in the
  kernels, and in C for items that only PostgreSQL can compare. The
  items are Tessera's encoding of SQL's order. This is the last and the
  smallest machine.
- **A build by several processes, and its rounds.** The phases of a
  shared build from `phases.rs`, together with what only C decides
  today: the agreement on a split and the publishing of the filter
  before it; the catch-up of a participant that joins a level late;
  whether the phases that write to disk have work; the partitions kept
  in memory; the rounds over the partitions on disk; which partition
  goes to one participant alone; the stop of a participant that leaves
  early; and who frees what. Its shared state lives in words that the
  host places in shared memory. The barriers stay the host's: a step
  returns "wait" where the node waits today.

The whole build moves, not only the rounds. The phases that write to
disk exist only for the spill, and otherwise the hand-over from the
build to the rounds would sit on the boundary of the crate. The cost: a
shared build that never spills still steps a crate named for spilling,
and its counts of records and chunks are the table's. The path of such
a build is measured whenever the machine changes it.

- **Refused: the phases without a spill in `relhash`, and those with a
  spill in `relspill`.** That makes two machines over one barrier, with
  the hand-over between them on the boundary of two crates.
- **Refused: the decisions as separate calls**, as today. That keeps
  the order of the decisions, their state and their mistakes in C,
  where no model reaches them.
- **Refused: one machine for every operator.** Their protocols have
  different states. What they share, the level plan, the spill words
  and the file of a set, the machines share as parts.
- **Refused: async Rust.** A future is a state machine the compiler
  writes. It is hard to keep in shared memory, to bound, or to prove.

### How the machines work together

A shared join runs two machines in each participant.

- The **shared-build machine** owns the words in shared memory, the
  phases, the rounds, and which partition goes to whom. It decides
  when the work is complete, and it cleans up.
- The **join machine** owns the participant's own level, its pieces and
  its passes. It never frees a shared region.

```
  shared-build machine                   join machine
  --------------------                   ------------
  partition p taken alone  -- p ----->   split, joined whole, or
                                         joined in pieces over passes
                           <-- done --   of its outer rows
  the next partition?      <-- ask ---   (a serial join picks its
                           -- q ----->    next partition itself)
  rounds over the partitions on disk, a round for the rows probed in
  memory, then the frees
```

Three hand-overs cross between them. Each is an action of one machine
and an event of the other:

- A partition that one participant takes whole goes to its join
  machine. The join machine joins it as a serial join joins a partition
  read back, and reports it done.
- In a shared join, when the join machine needs its next partition, it
  asks the shared machine.
- The tail of the rows kept in memory, probed before the rounds, is a
  round of its own.

Faults of the shared join sat on such hand-overs before: a late
participant that zeroed the shared values, and a RIGHT join's last
participant that missed one that had stopped. The loom models and the
property tests cut a step at each hand-over.

### One owner for each rule

A reading of the code found these rules written more than once. Each
moves into the crate once, and its copies go.

- The loop that sends partitions to disk: in the join, the shared join
  and the grouping.
- The setting up of a level, in the same three: its partitions, its
  chunks, its words, the expected bytes of level 0, and the shift of a
  level below.
- The rule of a full chunk: twice in the join, once in the grouping.
- The partition of a hash: an inline function in C, and one in
  `relhash`.
- The bits of the hash each level takes. The join and the grouping's
  groups take the low bits, from level 0 up; the grouping's rows take
  the high bits, down. Three rules, which the level plan holds.
- The writer and the reader of a chunk of columns: for the grouping's
  rows, twice for the sort's runs, for the join's outer rows, and for
  the gather of parallel workers.
- Which partitions to open, and where outer rows go: in the join and
  the shared join.
- The start of the grouping's spill at seven eighths of `hash_mem`: in
  three places of the node, in its estimate in the planner, and in a
  setting.

Two copies differ. The join gives a level below what is left of its
limit, or a quarter of it; the grouping gives every level all of
`hash_mem`. A step that moves a rule keeps what SQL sees: where copies
differ, the machine takes each node's rule as a parameter, and a task
of its own makes one rule, with a measurement.

Two rules of the join look alike but are not copies. The setting
`tessera.join_spill_split_room` decides whether a partition read back
is split into a level below. Two thirds of the free room decide how
large a piece is when a partition is joined in pieces; the last third
keeps room for the piece's index. A test of the suite `join` sets the
setting high, so that partitions are joined in pieces and none is
split. The machine keeps both rules, as two parameters.

### The spill checked first

Before any code moves, outside reviews read the spill as it is, as
they did for `relhash`. They get the spill's code in C and in Rust, its
specs and its designs, and they are asked for faults that change a
result, crash, leak or hang. Each accepted finding becomes a task,
fixed with a test that fails before the fix. Moving code with its
faults would make them harder to find: a move keeps behavior, so a
fault would move with it.

A reading of the code and two outside reviews of this proposal have
already found some faults. Each is to be shown by a test first.

- **An expanded value copied as a pointer.** The grouping's rows copy a
  value by reference by its size alone, while the join flattens an
  expanded value first. So an expanded value in the rows of a grouping
  that spills is copied as a pointer, into memory that is freed by the
  time the rows are read back. The gather of parallel workers writes
  rows through a writer that copies such a value the same way, and may
  send a pointer into one process's memory to another. One copy of a
  value by reference, which flattens it, is to serve every writer,
  its count of bytes included. The three copies of today, the join's,
  its spill's and that of `runtime/rows.c`, become this one.
- **A chunk read by the wrong shape.** The sort reads a chunk of
  columns read back with its own count of columns, and never compares
  it with the shape the chunk's bytes give: a damaged chunk makes it
  read past its memory. The join reads any outer block that is not a
  block of values as columns, without checking its kind.
- **An overflow.** A block of values numbered 2^30 passes the join's
  guard, and the loop that grows its table of value chunks then
  overflows an `int`. A smaller number asks for a table far larger than
  any file.
- **A partition dropped too early.** A partition of a shared set that
  is dropped before the set is finished is left out of the trailer,
  while its blocks stay in the file. No node does this, but nothing
  refuses it.
- **Memory that nobody counts,** above.

The outside reviews of the spill as it is found more. Each is a task
of its own, shown by a test first:

- **An expression key written as an address.** A join keeps a key
  that is an expression, such as `lower(t)`, as a word on disk. For a
  key of text that word is the address of the value, which is freed
  by the time the partition is read back: the server crashes.
- **A queue that runs over.** A partition whose values outgrew a
  chunk is queued again each time it fills before the queue is
  emptied, and a split never empties it: the queue writes past its
  array.
- **No more than 64 outer columns.** A join that spills cannot keep
  more outer columns, though the format can, and fails the query.
- **A shared partition sized by its records.** A partition that one
  participant joins alone is sized without its values, so a partition
  of long values is loaded whole, far past `hash_mem`.
- **Empty buckets.** The grouping's spilled rows take their partition
  from the high bits of the hash, which the buckets of a partition's
  table take too, so most buckets stay empty and chains grow long.
- **A piece that never ends.** A join ends a piece of a partition at
  a block of values; a partition whose values are all NULL has none,
  and is loaded whole.
- **Long loops without a check for a cancel,** in the shared build,
  its rounds, the first split of a table and the demotion of a
  partition.
- **More reads that trust a file,** in the join, the sort and the
  grouping, which join the damaged bytes above, one task a node.
- **Spill words that break their promise** after an update they
  refuse.

Two suspicions turned out not to be faults. One is the join's two
thresholds, above. The other is a block of columns that flushes the
buffer before its header is checked: it flushes only blocks that were
checked before it. Its comment is wrong.

The tests of damaged bytes run through the nodes' reads as well as the
crate's. What a node reads of a chunk, its kind, its shape and the
references to values, comes from the crate, checked against what the
node expects. So a damaged file reaches no C that trusts it.

The change also takes the findings of `spill-format` that waited for a
decision:

- A reader that opens a participant's file before its first write gets
  "no blocks", without an error. The guard against it is the nodes'
  barriers, which the machine of several processes now holds.
- A join checks the chunk that a reference to a value names, but not
  the byte in it, and no node checks that a value's own length ends
  inside its chunk.
- The order of blocks in a partition, values before the blocks that
  refer to them, is a rule written only in a design. A machine makes
  it a rule of its own, and a test shows it.
- The single form of a reference to a value cost the sort and the
  grouping of text 1.0 to 2.6 % in a run on battery power, below what
  the A/B tool flags. The counter benchmark of task 2.6 counts it, and
  a run on mains power is still owed.
- `docs/spill.md` and the comments of the reserves count a buffer of
  one page for each partition's file, though a set has one buffer. The
  reserves move into the level plan, which says what they hold.

### Errors of two kinds

An error of the crate is one of two kinds:

- a **wrong call**, such as an event a machine does not expect in its
  state, or a partition past the level's count;
- **damaged bytes**, read back from a file.

The error names the rule it broke, in one word of 64 bits, as
`relhash`'s errors do. Tessera turns the first kind into `XX000` and
the second into `XX001`, as it does today. A test sees which rule
refused, so it can show that each check is needed.

A limit of the host, such as a row longer than PostgreSQL allows, is
neither kind: it stays the host's error, with its own SQLSTATE. A call
that needs more memory, and an action the host could not do, are
outcomes of the protocol, not errors of the crate.

### Any bytes, a bounded read

The promise: whatever bytes a file holds, a read of the crate returns
the data or an error. It never crashes, never loops without end, and
never asks for a region past the bounds its caller gave.

The caller gives a bound for each kind of block: for a chunk of records
or of columns, by the plan of its level; for a block of values, by the
largest row the node writes. A read asks at most for one block of its
kind, with its header and the slack of packing, or for one page of a
list, and never for more than the file holds. The promise holds for a
file that nobody writes while it is read: a participant reads another's
file only after the barrier that ends the writing of that file.

Damaged bytes that still pass every check read as other data, since a
block has no checksum (see "What we do not do"). The promise is that a
read goes wrong in no other way.

Every node passes the real bounds of its blocks, instead of the largest
allocation PostgreSQL allows, so that the bound means something. A
reader of a shared set takes the bounds its writers agreed on.

The promise is shown as `relhash`'s is:

1. **Trust points.** Every read of bytes from a file goes through one
   layer and carries a mark. A script fails on a read without a mark,
   and on a read that goes around the layer. The list of trust points
   says, for each, what the bytes decide (an offset, a length, the
   count of a loop) and which check, error, proof and row of the damage
   matrix covers it.
2. **Kani.** Proofs, for all values, of the checks of a header, a
   trailer and the lists; of the bounds of unpacking records and
   columns; and of the arithmetic of a level plan and of the rule that
   picks a partition to send to disk. They come in families, and each
   family names, in the list of trust points, the checks it covers.
3. **The damage matrix.** Files written by the crate, with every field
   of a header, a trailer, a list and a packed body damaged at and
   around its limits, against every read of the crate and of the
   nodes. A test checks the rule that each error names.
4. **Fuzzing.** Any bytes as a file; and any sequence of events and
   numbers given to a machine, which must answer with an action or a
   wrong call's error, never with a panic.
5. **Mutants.** Every mutant of the crate is caught by the crate's own
   tests, or listed with the reason it is equivalent.
6. **Miri** over all the crate's tests and over the tests of the C
   entry points of the spill. Miri cannot run PostgreSQL's C; the
   sanitizers of CI and the tests of errors and cancels cover that.

### The machines against a model

A property test runs each machine with a host of its own: files are
vectors, memory is a count, and the table is `relhash`'s owning table.
The test picks the rows, their keys and duplicates, the memory limit,
and the moments when an action fails. The same rows go through a plain
operator in memory, a join, a grouping or a sort with no spill. Then:

- a join must give the same pairs, each as many times;
- a grouping must give the same groups and states;
- a sort must give the same items, each no greater than the next;
  equal items may come in any order.

The files must be empty at the end. The memory a machine decides by
must stay under its limit, plus what its rule allows; the states of
generic aggregates, which grow outside the machine, are left aside. The
test checks the actions too: no partition joined twice, a piece's rows
loaded once in a pass, and no action before the one it depends on is
done.

Hand-written cases cover what a random test rarely reaches, and each
checks that its path really ran: a join's pairs of every multiplicity,
with and without a condition beside the keys; nested levels; the tails
of pieces; output held across calls; a writer with no rows; an early
stop; the hand-over from a build to its rounds; both spills of a
grouping; keys numbered by a dictionary; aggregates of computed values;
and a sort read again, read backward, and of text, with its output kept
across calls.

Some cases are states that today's code leaves wrong but that no path
reaches yet; a machine must refuse them or handle them: a grouping
asked for rows after its last partition, whose table still names
freed chunks; a participant stepped again after it finished, which
the build itself does not reset; and a build that follows a round on
the same state, whose count of value slots the round leaves behind.

The machine of several processes runs under loom with two and three
participants: its phases, its words, its files in memory, its catch-up,
its hand-over to the rounds, its stop and its frees. Today's models
keep running; the new ones cover what they leave out.

### The specs change with the code

`hash-table` and `spill-format` promise calls of single decisions, and
a format, that the crate takes over. Each task that replaces such a
call, or moves a rule of the format, edits those specs in the same
commit. The requirement is written again for the call that takes its
place, or it moves to `relspill`, with its tests, and joins this
change's requirements there. So no spec ever names a call or a test
that is gone, and no separate change has to catch up with the code. At
the end, the requirements of `relspill` are grouped by topic.

`spill-format` keeps the C API of a set and what SQL sees: the errors
and their SQLSTATE, the limit of temporary files, the tablespaces, and
a file removed after an error. `hash-table` keeps what Tessera builds
around the table.

### The crate's documentation

As for `relhash`: complete by itself; every public item documented; the
rules, layouts and limits of the API written in it; the README as the
crate's front page; built without warnings, with its examples run in
CI. The spec's requirements link to the items. Each task documents
what it adds, and from its first task the crate refuses a public item
without documentation.

### The crate's contract

The crate is a new part, so it gets an architecture contract, kept in
this change's folder until the change closes. Its rules keep the crate:

- free of Tessera and of SQL;
- free of input and output, and of allocation;
- with its reads of bytes behind one layer;
- free of unsafe code;
- the only owner of each rule of a spill;
- with a small public surface, and at no cost to Tessera.

The words in shared memory come to the crate as a slice of atomic words
that its host makes, so the unsafe code that attaches them stays in
`tessera-capi`. Most rules are `pending` until the task that writes
their check. The contract's forecast names the likely changes: a merge
join whose inputs are sorted through the runs; a host with another
manager of temporary files, or one that is not PostgreSQL; a new kind
of block; a change of `relhash`'s record; and a grouping by several
processes that spills. A probe of each runs once the crate exists.

### Speed

Every commit is one of two sorts, as for `relhash`.

- **A move.** The machine code of every benchmark program, and of both
  modules, `tessera_nodes` and `tessera_kernels`, is the same as the
  base's. The check is `tessera-bench --base REF --disasm`, also with
  `--module nodes` and `--module kernels`, and `cargo ab moved` for a
  file that is cut in two. Most decisions that the machines take over
  are compiled into the nodes.
- **A change.** First the machine code: a loop may grow only by a check
  the promise needs, written down with its reason. Then the counters of
  the processor, on the new benchmark of the spill's kernels and of
  each machine's steps, which comes with the machine; they count only
  on macOS for now, so they run on a developer's Mac, not in CI. Then
  `cargo ab measure` on the spill family of `bench/pg`, on the cases
  `runs_text` and `rows_text` of the exec family, and on the queries
  the machine changes: a shared join with and without a spill, since
  its phases move; the rounds of a parallel join; a skewed join read in
  pieces; a sort that merges in several passes. The counters and the
  times are judged by the thresholds of the measurement plan.

A machine steps once for a chunk, a block or a partition, not for a
row. Its cost is counted by the benchmark, not assumed.

### How the work goes

Each task is one pull request into `main`. A task brings its evidence
with it: the model, the property test, the cases of damage and the
documentation of what it adds come in the same task as the code, not
later. A task that moves a file named by a spec or a design edits those
paths in the same commit.

## Measurement plan

- **What:** the machine code; the instructions of the spill's kernels
  and of the machines' steps; and the time of SQL queries that spill.
- **Tools:**
  - `tessera-bench --base REF --disasm` on every program, and with
    `--module nodes` and `--module kernels`, for every commit;
  - a new program of `tessera-bench` with counters for packing and
    unpacking records and columns, appending by partition, and the
    steps of each machine, added with the machine;
  - `cargo ab measure` on the family `spill` of `bench/pg` (joins and
    groupings at a `work_mem` of 4, 16 and 64 MB), on the cases
    `runs_text` and `rows_text` of `exec`, on the cases of `join` that
    spill, on a parallel join with and without a spill, on a skewed
    join read in pieces, and on sorts that merge in several passes.

  The cases `runs_text` and `rows_text` are run once on mains power
  before the crate, as the base for the cases of text.
- **Thresholds:** those of `docs/measuring.md`. A move must show the
  same machine code. A change fails at more than 1 % more instructions,
  or at cycles past their limits. A confirmed excess starts the search
  that `CONTRIBUTING.md` describes under "Stop and ask". If the search
  does not remove it, the work stops, and the pull request asks what to
  do.

## What we do not do

- **File input or output in the crate.** See "No file input or
  output".
- **Reading ahead.** A read asks for one block. The host may read
  ahead; the crate does not ask it to.
- **Compressing blocks.** Refused before: lz4 runs at 1.3 GB/s, where
  the packing by lanes runs at 7 to 10 GB/s.
- **Checksums of blocks.** Damaged bytes that still look like a block
  give a wrong answer without an error. Only a checksum read with every
  block would catch them, at a cost on every read.
- **Spilling the states of sums instead of rows.** A separate entry of
  the roadmap, to be decided by a measurement.
- **Retrying an action that failed.** See "When an action fails".
- **Work for a participant that comes after the build.** One that
  attaches to a round in progress takes part in it. One that comes
  after the in-memory part of the build is freed finds no work, though
  rounds still run. Refused before: it would need a path that only a
  participant late by just that much takes, and no test reaches it.
- **Values by reference in the crate.** Their size, their copying and
  expanded values belong to PostgreSQL.
- **A spill for a merge join.** The forecast names it; nothing builds
  it now.
- **Publishing on crates.io**, and **32-bit targets**, as for
  `relhash`.

## Risks / Trade-offs

- [The machines rewrite about 7,200 lines of C, where a wrong result
  hides under memory pressure] → today's faults are found and fixed
  first; one machine a task; property tests against a model; the SQL
  suites and `make crosscheck` with spills in every task; a move keeps
  the plans and the outputs.
- [The join gives control back to the executor in the middle of a
  partition, to give out a tail or to hold a batch] → the machine has
  those states, as the node has them now, and the model test cuts a
  step there.
- [An ERROR of PostgreSQL in the middle of an action] → the crate calls
  nothing of PostgreSQL, so no jump crosses its frames. The failed
  action ends the spill, and the host frees or resets its state, as
  "When an action fails" says. A machine is plain data in a memory
  context and is freed with it, as the node's state is today.
- [A machine too large for one review] → the join's machine comes in
  four tasks and the shared build's in three, each one state or one
  hand-over, and each keeps the passes, splits and batches that EXPLAIN
  shows in the suite `join`.
- [The machine of several processes is too large for loom] → a model
  for each hand-over, as the models of the build and of the rounds are
  separate today, each with a bounded number of preemptions.
- [A machine's step costs more than the C it replaces] → steps happen
  per chunk or per block; the new benchmark counts them and `cargo ab`
  times them.
- [Making one rule of two copies changes plans] → the copies move as
  parameters first; one rule is made only in a task of its own, with a
  measurement.
- [The change is long, and `main` moves on] → each task is a pull
  request into `main`; nothing waits on a branch.

## Migration Plan

`include/tessera/runtime_spill.h` keeps its calls. The calls of single
decisions in `include/tessera/spill.h`, in `include/tessera/table.h`
and in the kernels' table give way to the calls of a set and of the
machines. Each goes in the task that moves its user, and that task
edits the specs of those calls in the same commit. No setting, plan or
result changes, except where a fixed fault says what changes. The
format on disk does not change. A file lives only as long as its query,
so no file of an older build is ever read. Each task can be reverted
on its own.
