## Context

This design is about the move. Its folder goes when the change closes,
and what a reader of the crate needs is then written into the design
of `relspill`; nothing of this context goes there.

A spill has four kinds of code today.

- **The format, in Rust.** `crates/tessera-spill` (1,719 lines) writes
  and checks the header of a block, packs a chunk of `relhash` records
  by lanes of 4 bytes, packs a chunk of columns by the span of each
  lane, and plans a level: how many partitions, how long a chunk. It
  depends on `anyhow` alone and has no unsafe code. Its packing of
  records copies `relhash`'s layout of a record as bare numbers.
- **The shared words and the phases, in the kernels.**
  `tessera-kernels/src/shared_spill.rs` (1,055 lines) keeps the words
  of a spill: the bytes and records of each partition, its flags, the
  budget, and the rule that picks the partition to send to disk; one
  code over atomic words in shared memory and over plain words of one
  process. `phases.rs` (842) steps a participant of a shared build
  through its phases and its rounds. `loom.rs` (635) models both.
  `spill_columns.rs` (606) appends rows to chunks of columns.
- **The entry points, in `tessera-capi`.** `c/spill.rs` (670) and
  `c/shared_spill.rs` (522) give C the calls above; `c/spill.rs` also
  reads PostgreSQL's values by reference, with unsafe code that no test
  under Miri runs: Miri runs the tests `columns` and `table`, which
  append, pack and unpack chunks of columns and step the phases, not
  the tests `spill`.
- **The protocol and the files, in C.** `runtime/spill.c` (945) calls
  PostgreSQL's files, keeps the write buffer and the lists of blocks,
  and writes and checks the trailer of a shared file. The nodes decide
  the rest: `hashjoin_spill.c` (2,344), `hashjoin_shared.c` (1,698),
  `agg_spill.c` (1,371), `sort_external.c` (878).

The maintainer's answers of 2026-10-28 draw the line: the crate holds
the format of the whole file without doing input or output; the
protocol of levels, pieces and rounds becomes state machines in the
crate; the crate depends on `relhash`; it is named `relspill`.

## Goals / Non-Goals

**Goals:**

- The spill as a crate that a database engine outside Tessera can take
  with `relhash`: its host gives memory and does the input and output.
- Every rule of a spill written once, in the crate.
- Faults of the spill as it is found and fixed before it moves.
- The promise "any bytes, a bounded read" for a file, shown by proofs
  and tests that cover every place the crate trusts bytes it reads.
- The machines shown correct against a model of the operator without a
  spill, and the machine of several processes under loom.
- Not one instruction more in a hot loop of Tessera; a counter
  benchmark for the kernels of a spill.

**Non-Goals:**

- See "What we do not do".

## Decisions of the maintainer

On 2026-10-28, after a survey of the spill:

- The crate is named `relspill`, a pair to `relhash`.
- It holds the format of a file, the lists of blocks and the trailer
  included, and does no input or output: it says which bytes to write
  where, and which to read.
- The protocol of levels and pieces, the reading of partitions back,
  and the rounds of a shared build become state machines in the crate.
- The crate depends on `relhash` for the layout of a record, so that a
  change of the layout fails to build instead of damaging data.

Carried over from `relhash` as the same kind of crate: 64-bit targets
only; documentation complete by itself; `publish = true` as a mark of
the surface, with no publishing until the maintainer decides it.

The same day, after two outside reviews of this proposal:

- The join's two thresholds stay two. The setting decides whether a
  partition read back splits; two thirds of the room left decide how
  large a piece is. They are two rules, not a fault.
- An action of the host that fails ends the spill. The crate does not
  retry it; the state it leaves is only freed or reset.
- The faults the reviews found in the code are tasks of the
  validation.
- The specs of `hash-table` and `spill-format` change in the task that
  changes a call they promise, not in changes of their own afterwards.
- `relhash` opens the layout of a record as a small public API, an
  early task of the crate, so that `relspill` packs records by it.

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
  | plan of a level                |  |   measure of what is allocated |
  | words of a spill, the rule of  |  | barriers, interrupts, errors   |
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

The test of a line: a rule that a database engine without PostgreSQL
would need to spill with `relhash` goes into the crate; a rule of SQL,
of PostgreSQL's types or of its executor stays.

- In the crate: `tessera-spill` whole; `shared_spill.rs`, `phases.rs`
  and their loom models; `spill_columns.rs`, whose traits already take
  rows and values from a source; the bookkeeping of `runtime/spill.c`;
  every decision of the nodes listed under "One owner for each rule".
- In Tessera: `DatumRows` of `c/spill.rs`, the source of rows over
  PostgreSQL's values; the copying of values by reference in the nodes,
  which flattens an expanded value before any writer sees its bytes;
  the calls of `fd.c` and `SharedFileSet`; the measure of memory by the
  contexts; barriers, DSA, interrupts, errors; the settings, which
  become the parameters of a machine.

### No input or output

The crate never opens, reads or writes a file. Its host does, and tells
the crate what happened. This is the "sans-IO" form: a protocol is a
value that takes bytes and events and gives bytes and requests.

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

- **Why.** A call of PostgreSQL may raise an ERROR, which jumps over
  every frame between it and its handler. A crate that called the file
  functions through callbacks would have that jump cross its frames,
  which Rust does not allow for frames that own anything; every
  callback would need a `PG_TRY` around it. Sans-IO keeps every call of
  PostgreSQL in C. It also makes the crate testable whole: a file is a
  vector in a test, a fuzzer feeds it any bytes, a model runs it in
  memory, and Kani reasons about it without a file system.
- **Refused: callbacks for input and output.** Simpler for the crate,
  but the jump above, and a host that must be reentrant.
- **Refused: files left in C.** The trailer and the lists would stay a
  format that only C writes and checks, outside every proof and fuzzer.

### When an action fails

An action is outstanding from the step that returns it until the host
reports it done. While a write is outstanding, the host holds the bytes
it names in the crate's buffer, and the crate takes no call that would
change them; a region is replaced only when no action holds it.

An action can fail: a write that wrote part of its bytes, a read that
got fewer than it asked for, an error of the manager of files, the
limit of temporary files, a cancel. The crate does not retry and does
not recover. The host reports the failure, and the set or the machine
then takes no call but a free or a reset; any other is a wrong call.
In a shared build, a participant whose action failed never arrives at
a barrier as done, and its claims in the shared words are not
withdrawn: the host ends the whole parallel execution, as PostgreSQL
does when an ERROR in one process ends the others. A host that is not
PostgreSQL owes the same.

Three things look alike and are kept apart:

- The end of a file reached before a block ends: damaged bytes, an
  error of the crate.
- A participant that wrote no file: an empty set, which the host says
  only once that participant has finished; asked before, it is a wrong
  call.
- A read or a write the host could not do: the host's error, which ends
  the spill as above.

The host gives what only it knows: the length of a participant's file,
from which the crate finds the trailer, and which file is whose. It
keeps three ends of a set apart: releasing it, which closes its file and
keeps it for the others; freeing it, which deletes its file; and
resetting a shared set for a new execution, which deletes every file of
the set, comes only after every reader has let go, and may reuse the
names of the last execution's files.

The host's barrier orders memory: every write a participant made before
it arrives is seen by every participant after they leave. The words of
a spill use relaxed atomics where a barrier orders them, and the filter
of a shared build is published under a lock before the split is agreed.
The documentation of the machine says so, for a host that brings its
own barrier.

The tests cut the host's actions by failures between two writes, before
a set is published, and during a reset. They check that every region is
freed and every file is gone, not that a query's output is whole.

### The file of a set

A set is one file that holds the blocks of every partition of a level,
in the order they were written; a shared set is one such file for each
participant, read by the others. The crate takes over what
`runtime/spill.c` does besides calling PostgreSQL:

- **Writing.** The crate keeps the write buffer the host gives, packs a
  block into it or beside it, and keeps the list of each partition's
  blocks. A call that writes returns the ranges to write: the buffer
  when it is full, and a block larger than the buffer on its own. At
  the end of a shared set it returns the lists and the trailer.
- **Reading.** For a set of its own, the crate gives the position and
  length of each block from its lists. For another participant's file,
  the host gives the file's length; the crate asks for the trailer at
  its end, then the counts, then the entries of one partition, a page
  at a time, and checks each before it asks for the next: the same
  checks C does today, now in one place with the header's. The host
  reads each range and hands the bytes back.
- **What C keeps.** `runtime/spill.c` keeps its calls of `fd.c` and
  `SharedFileSet`, the limits PostgreSQL puts on temporary files, the
  names of the files, and the errors those calls raise. Its C API,
  `include/tessera/runtime_spill.h`, keeps its calls, so the nodes'
  use of a set does not change in the step that moves it.

The format on disk does not change in that step: a file written before
it reads after it.

### Memory the caller gives

A call of the crate allocates nothing, as in `relhash`. Every region it
uses, the write buffer, the lists of blocks, a reader's buffer, the
words of a spill and a machine's state, is given by the caller. A call
that cannot start without a larger region returns how much it needs and
changes nothing; the caller gives a larger region or fails. A call that
appends rows to a chunk of columns may make progress first: it appends
the rows that fit, says which, and returns what the next row needs, so
that a sort or a gather writes the chunk and goes on. So the memory of
a spill is allocated where PostgreSQL counts memory, and a machine can
ask its host for the measure it decides by.

This also makes the memory that nobody counts today visible: the lists
of blocks, which grow with the input; the matched rows of a join read
in pieces, which grow with its outer rows; and the block a join reads
ahead past a piece. Counting them changes when a join sends partitions
to disk, which an earlier attempt showed in five plans of the suite
`join`. So the step that moves the lists keeps them out of the measure,
as today, and a task of its own counts them: every region by its
capacity, and once, since the grouping already counts the contexts of
its spill whole; with a rule of what may go past the limit and by how
much, and of what a node does when memory runs out. A fixed reserve
cannot hold what grows with the input. That task names the plans and
the lines of EXPLAIN that change, and edits the specs that show them.

### The machines

A machine is the protocol of one node's spill, written as a state
machine in the crate. The node holds it and steps it: each step takes
an event and returns an action. The events are what the node saw: a
batch went into the table, a chunk of a partition is full, the input
ended, a block was read, the others arrived at a barrier. The actions
are what the node must do: give a partition a chunk, write a chunk,
free one, read the next block, wait, give out rows. A machine decides
by numbers the node passes: bytes, rows, the measure of memory, the
settings.

What a machine does not do: anything for each row. Appending rows by
partition, probing, copying values and writing their references stay
in `relhash`, the kernels and the nodes. Where a decision is made for
each row today, a machine decides for each partition and a kernel
applies it to the rows: the join's routing of an outer row is a
choice for its partition, kept in memory, on disk, or answered at once
because it has no inner rows.

The machines, each with the states the code has today:

- **The join's spill.** A build in memory; a build that spills at level
  0; probing the partitions kept in memory; joining the partitions on
  disk one by one, each split to a level below, joined whole, or joined
  in pieces over several passes of its outer rows; and the tails of
  RIGHT and FULL joins between them. It owns the decisions the node
  makes in C today: when to start, the limit of a level below, the
  shift of its hash, a chunk that is full, the routing of a partition,
  which partitions to open, whether to read in pieces, where a piece
  ends, the passes over the outer rows.
- **The grouping's spill.** Groups by partition, the partitions sent to
  disk, the partitions given out in two passes, each split to a level
  below or merged; and the rows of generic aggregates, frozen and
  replayed level by level. Two mechanisms with different rules for the
  bits of the hash; the move keeps both. The machine composes the two:
  they share the file of a set and the writer of columns, not their
  states.
- **The sort's runs.** A plan more than a machine of many states: when
  a run goes to disk, the sets of runs, the blocks and the merge's
  fan-in, the passes of the merge, and how long runs live: a rescan
  reads the last merge again, and a scan backward makes one final run.
  The merge of the items stays where it is, in the kernels and, for
  items only PostgreSQL can compare, in C: the items are Tessera's
  encoding of SQL's order. It is the last and the smallest machine.
- **A build by several processes and its rounds.** The phases of a
  shared build, from `phases.rs`, with what only C decides today: the
  agreement on the split and the publication of the filter before it,
  a participant's catch-up with a level it joined late, whether the
  phases that write to disk have work, the partitions kept in memory,
  the rounds over the partitions on disk, which partition goes to one
  participant alone, the stop of a participant that leaves early, and
  who frees what. Its shared state lives in words the host places in
  shared memory; the host's barriers stay the host's, and a step
  returns "wait" where the node waits today.

The whole build moves, not only the rounds. Its phases that write to
disk exist only for the spill, and the hand-over from the build to the
rounds would otherwise sit on the boundary of the crate. The cost: a
shared build that never spills steps a crate named for spilling, and
its counters of records and chunks are the table's. The path of such a
build is measured when the machine changes it.

- **Refused: the phases that do not spill in `relhash`, and those that
  do in `relspill`.** Two machines over one barrier, and the hand-over
  between them on the boundary of two crates.
- **Refused: the decisions as separate calls**, as today. That keeps
  the order of the decisions, their state and their mistakes in C,
  where no model reaches them.
- **Refused: one machine for every operator.** The protocols differ in
  their states; what they share is the plan of a level, the words and
  the file of a set, which the machines share as parts.
- **Refused: async Rust.** A future is a state machine the compiler
  writes, hard to keep in shared memory, to bound, or to prove.

### How the machines compose

A shared join runs two machines in each participant. The machine of
the shared build owns the words in shared memory, the phases, the
rounds and which partition goes to whom; it owns completion and
cleanup. A join machine owns the participant's own level, pieces and
passes, and never frees a shared region.

```
  shared build machine                   join machine
  --------------------                   ------------
  partition p taken alone  -- p ----->   split, joined whole, or
                                         joined in pieces over passes
                           <-- done --   of its outer rows
  the next partition?      <-- ask ---   (a serial join picks its
                           -- q ----->    next partition itself)
  rounds over the partitions on disk, a round for the rows probed in
  memory, then the frees
```

Three hand-overs cross between them, each an action of one machine
and an event of the other:

- A partition one participant takes whole goes to its join machine,
  which joins it as a serial join joins a partition read back, and
  reports it done.
- In a shared join, the join machine's choice of its next partition is
  a question to the shared machine.
- The tail of the rows kept in memory, probed before the rounds, is a
  round of its own.

Faults of the shared join sat on such hand-overs before: a late
participant that zeroed the shared values, a RIGHT join's last
participant that missed one that had stopped. The loom models and the
property tests cut a step at each.

### One owner for each rule

The survey found these rules written more than once. Each moves into
the crate once, and its copies go.

- The loop that sends partitions to disk: the join, the shared join,
  the grouping.
- The making of a level: its partitions, its chunks, its words. The
  join, the shared join, the grouping. And the expected bytes of level
  0, and the shift of a level below, in the same three.
- The rule of a full chunk: twice in the join, once in the grouping.
- The partition of a hash: an inline function of C and one of
  `relhash`.
- The bits of a hash each level takes: the join and the grouping's
  groups take the low bits from level 0 up, the grouping's rows the
  high bits down. Three rules, which the plan of a level holds.
- The writer and the reader of a chunk of columns: the grouping's rows,
  the sort's runs twice, the join's outer rows, and the gather of
  parallel workers.
- Which partitions to open, and the routing of outer rows: the join and
  the shared join.
- The start of the grouping's spill at seven eighths of `hash_mem`:
  three places of the node, its estimate in the planner, and a
  setting.

Two copies differ: the join gives a level below what is left of its
limit, or a quarter of it; the grouping gives every level the whole of
`hash_mem`. A step that moves a rule keeps what SQL sees: where copies
differ, the machine takes each node's rule as a parameter, and a task
of its own makes one rule, with a measurement.

Two rules of the join look alike and are not copies. The setting
`tessera.join_spill_split_room` decides whether a partition read back
splits to a level below; two thirds of the room left decide how large a
piece of a partition joined in pieces is, which keeps room for its
index. A test of the suite `join` sets the setting high, so that
partitions are joined in pieces and none splits. The machine keeps
both, as two parameters.

### The spill validated first

Before any code moves, the spill as it is is read by outside reviews,
as `relhash` was: models of other makers, given the code of the spill
in C and in Rust, its specs and its designs, and asked for faults that
change a result, crash, leak or hang. Each finding the maintainer
accepts becomes a task, fixed with a test that fails before the fix.
Moving code with its faults would make them harder to find: the move
keeps behavior, so a fault would move with it.

The survey and two outside reviews of this proposal found some
already, each to be shown by a test first:

- The grouping's rows copy a value by reference by its size alone; the
  join flattens an expanded value first. An expanded value in the rows
  of a grouping that spills is copied as a pointer to memory freed by
  the time the rows are read back. The gather of parallel workers
  writes rows through a writer that copies such a value the same way,
  and may send a pointer into one process's memory to another. One
  copy of a value by reference, which flattens it, is to serve every
  writer, the count of its bytes included; the three copies of today,
  the join's, its spill's and that of `runtime/rows.c`, become it.
- The sort reads a chunk of columns read back by its own count of
  columns, never compared with the shape the chunk's bytes give: a
  damaged chunk makes it read past its memory. The join reads any outer
  block that is not one of values as columns, without checking its
  kind.
- A block of values numbered 2^30 passes the join's guard, and the loop
  that grows its table of value chunks then overflows an `int`; a
  smaller number asks for a table far larger than any file.
- A partition of a shared set dropped before the set is finished is
  left out of its trailer while its blocks stay in the file. No node
  does this; nothing refuses it.
- The memory nobody counts, above.

Two suspicions of the survey were not faults: the join's two
thresholds, above; and a block of columns that flushes the buffer
before its header is checked, which flushes only blocks checked before
it. Its comment is wrong.

The tests of damaged bytes run through the nodes' reads as well as the
crate's. What a node reads of a chunk, its kind, its shape and the
references to values, comes from the crate checked against what the
node expects, so that a damaged file reaches no C that trusts it.

And the findings of `spill-format` that waited for a decision, which
this change takes:

- A reader that opens a participant's file before its first write gets
  "no blocks" without an error; the guard is the nodes' barriers, which
  the machine of several processes now holds.
- A join checks the chunk a reference to a value names, not the byte in
  it, and no node checks that a value's own length ends inside its
  chunk.
- The order of blocks in a partition, values before the blocks that
  refer to them, is a rule in a design only; a machine makes it a rule
  of its own, and a test shows it.
- The one form of a reference to a value cost the sort and the
  grouping of text 1.0 to 2.6 % in a run on battery power, below what
  the A/B tool flags; the counter benchmark of 2.6 counts it, and a run
  on mains power is owed.
- `docs/spill.md` and the comments of the reserves count a buffer of a
  page for each partition's file, though a set has one buffer; the
  reserves move into the plan of a level, which says what they hold.

### Errors of two kinds

An error of the crate is either a wrong call, such as an event a
machine does not expect in its state or a partition past the level's
count, or damaged bytes, read back from a file. It names the rule it
broke, in one word of 64 bits, as `relhash`'s errors do. Tessera turns
the first into `XX000` and the second into `XX001`, as it does today.
The error a test sees is the rule, so a test can show that each check
is needed.

A limit of the host, such as a row longer than PostgreSQL allows, is
neither: it stays the host's error, with its own SQLSTATE. A call that
needs more memory and an action the host could not do are outcomes of
the protocol, not errors of the crate.

### Any bytes, a bounded read

The promise: whatever bytes a file holds, a read of the crate returns
an error or the data, never a crash, a loop without end, or a request
for a region past the bounds its caller gave. The caller gives a bound
for each kind of block: a chunk of records or of columns by the plan
of its level, a block of values by the largest row the node writes. A
read asks at most for one block of its kind, with its header and the
slack of packing, or for one page of a list, and never for more than
the file holds. The promise holds for a file nobody writes while it is
read: a participant reads another's file only after the barrier that
ends its writing.

Bytes damaged into other bytes that pass every check read as other
data, since a block has no checksum (see "What we do not do"). The
promise is that a read goes wrong in no other way.

Every node passes the bounds of its blocks instead of the largest
allocation PostgreSQL allows, so the bound holds; a reader of a shared
set takes the bounds its writers agreed on.

The promise is shown as `relhash`'s is:

1. **The trust points.** Every read of bytes from a file goes through
   one layer and carries a mark; a script fails on a read without one,
   and on a read that goes around the layer. The list says for each
   what the bytes decide (an offset, a length, a count of a loop) and
   which check, error, proof and row of the matrix covers it.
2. **Kani.** Proofs for all values of the checks of a header, a
   trailer, the lists, and the bounds of unpacking records and columns,
   and of the arithmetic of a level's plan and of the rule of eviction.
   They come in families, each naming in the list of trust points the
   checks it covers.
3. **The matrix of damage.** Files written by the crate, with every
   field of a header, a trailer, a list and a packed body damaged at
   and around its limits, against every read of the crate and of the
   nodes. A test checks the rule of the error.
4. **Fuzzing.** Any bytes as a file, and any sequence of events and
   numbers given to a machine, which must answer with an action or a
   wrong call's error, never a panic.
5. **Mutants.** Every mutant of the crate caught by its own tests, or
   listed with the reason it is equivalent.
6. **Miri** over all the crate's tests and over the tests of the C
   entry points of the spill. Miri cannot run PostgreSQL's C: the
   sanitizers of CI and the tests of errors and cancels cover it.

### The machines against a model

A property test runs each machine over a host of its own: files as
vectors, memory as a count, `relhash`'s owning table. It chooses the
rows, their keys and duplicates, the limit of memory, and the moments
an action fails. The same rows go through a plain operator in memory:
a join, a grouping, a sort with no spill. A join must give the same
pairs, each as many times; a grouping the same groups and states; a
sort the same items, each no greater than the next, equal ones in any
order. The files must be empty at the end, and the memory a machine
decides by must stay under its limit plus what its rule allows; the
states of generic aggregates, which grow outside the machine, aside.
The test checks the actions too: no partition joined twice, a piece's
rows loaded once in a pass, no action before the one it depends on is
done.

Cases written by hand show what a random test reaches rarely, and each
asserts that its path ran: a join's pairs of every multiplicity, with
and without a condition beside the keys; nested levels; the tails of
pieces; output held across calls; a writer with no rows; an early stop;
the hand-over from a build to its rounds; both spills of a grouping,
keys numbered by a dictionary, aggregates of computed values; a sort
read again, read backward, of text, with its output kept across calls.

The machine of several processes runs under loom with two and three
participants: its phases, its words, its files in memory, its catch-up,
its hand-over to the rounds, its stop and its frees. The models of
today keep running; the new ones cover what they leave out.

### The specs, changed with the code

`hash-table` and `spill-format` promise the calls of single decisions
and the format that the crate takes over. Each task that replaces such
a call, or moves a rule of the format, edits those specs in the same
commit: the requirement is written again for the call that takes its
place, or leaves for `relspill` with its tests and joins this change's
requirements there. So no spec names a call or a test that is gone,
and no change of its own follows the code. At the end the requirements
of `relspill` are joined by topic.

`spill-format` keeps the C API of a set and what SQL sees: the errors
and their SQLSTATE, the limit of temporary files, the tablespaces, a
file removed after an error. `hash-table` keeps what Tessera builds
around the table.

### The documentation of the crate

As `relhash`'s: complete by itself, every public item documented, the
rules, layouts and limits of the API in it, the README as the crate's
page, built without warnings and its examples run in CI. The spec's
requirements link to the items. Each task documents what it adds; the
crate denies an item without documentation from its first task.

### The contract of the crate

The crate is a new part, so it gets a contract, in this change's folder
until the close. Its rules keep it free of Tessera and of SQL, free of
input and output, free of allocation, its reads of bytes behind one
layer, free of unsafe code, each rule of a spill in it alone, its
surface small and its cost to Tessera nil. The words in shared memory
come to the crate as a slice of atomic words its host makes, so the
unsafe code that attaches them stays in `tessera-capi`. Most rules are
`pending` until the task that writes their check. Its forecast names
the likely changes: a merge join whose inputs are sorted by the runs, a
host with another manager of temporary files or one that is not
PostgreSQL, a new kind of block, a change of `relhash`'s record, and a
grouping by several processes that spills. A probe of each runs once
the crate exists.

### Speed

Every commit is one of two sorts, as in `table-crate`.

- **A move.** The machine code of every benchmark program and of both
  modules, `tessera_nodes` and `tessera_kernels`, is the same as the
  base's: `tessera-bench --base REF --disasm`, and with `--module
  nodes` and `--module kernels`, with `cargo ab moved` for a file that
  is cut. Most decisions the machines take over compile into the
  nodes.
- **A change.** First the machine code, where a loop may grow only by a
  check the promise needs, written down with its reason; then the
  counters, run by the maintainer, on the new benchmark of the spill's
  kernels and of each machine's steps, which comes with the machine;
  then `cargo ab measure` on the spill family of `bench/pg`, on the
  cases `runs_text` and `rows_text` of the exec family, and on the
  queries the machine changes: a shared join with and without a spill,
  since its phases move; the rounds of a parallel join; a skewed join
  read in pieces; a sort that merges in several passes. The counters
  and the times are judged by the thresholds of the measurement plan.

A machine steps once for a chunk, a block or a partition, not for a
row: its cost is counted by the benchmark, not assumed.

### How the work goes

Each task is a pull request into `main`, read by the maintainer. A
task brings its evidence with it: the model, the property test, the
cases of damage and the documentation of what it adds come in the task
that adds the code, not after. A task that moves a file named by a
spec or a design edits those paths in the same commit.

## Measurement plan

- **What:** the machine code and the instructions of the spill's
  kernels and of the machines' steps, and the time of SQL queries that
  spill.
- **Tools:** `tessera-bench --base REF --disasm` on every program and
  with `--module nodes` and `--module kernels`, every commit; a new
  program of `tessera-bench` with counters for packing and unpacking
  records and columns, appending by partition, and the steps of each
  machine, added with the machine; `cargo ab measure` on the family
  `spill` of `bench/pg` (joins and groupings at a `work_mem` of 4, 16
  and 64 MB), the cases `runs_text` and `rows_text` of `exec`, the
  cases of `join` that spill, a parallel join with and without a
  spill, a skewed join read in pieces, and sorts that merge in several
  passes. The cases `runs_text` and `rows_text` are run once on mains
  power before the crate, as the base of the text cases.
- **Thresholds:** those of `docs/measuring.md`. A move must show the
  same machine code. A change fails at more than 1 % instructions, or
  at cycles past their limits. A confirmed excess starts the search of
  "Stop and ask"; one the search does not remove goes to the
  maintainer.

## What we do not do

- **Input or output in the crate.** See "No input or output".
- **Reading ahead.** A read asks for one block; the host may read
  ahead, the crate does not ask it to.
- **Compression of blocks.** Refused before: lz4 runs at 1.3 GB/s where
  the packing by lanes runs at 7 to 10.
- **Checksums of blocks.** Damaged bytes that still look like a block
  give a wrong answer without an error. Only a checksum read with every
  block would catch them, at a cost on every read.
- **Spilling the states of sums** instead of rows: an entry of the
  roadmap, taken by measurement.
- **Retrying an action that failed.** See "When an action fails".
- **Work for a participant that comes after the build.** One that
  attaches to a round in progress takes part in it; one that comes
  after the part of the build in memory is freed finds no work, though
  rounds still run. Refused before: a path that only a participant late
  by just that much takes, which no test reaches.
- **Values by reference in the crate.** Their size, their copy and an
  expanded value are PostgreSQL's.
- **A spill for a merge join.** The forecast names it; nothing builds
  it now.
- **Publishing on crates.io**, and **32-bit targets**, as for
  `relhash`.

## Risks / Trade-offs

- [The machines rewrite about 7,200 lines of C, where a wrong result
  hides under memory pressure] → the faults of today found and fixed
  first; one machine a task; the property tests against a model; the
  SQL suites and `make crosscheck` with spills in every task; a move
  keeps the plans and the outputs.
- [The join's output gives control back to the executor in the middle
  of a partition, to emit a tail or to hold a batch] → the machine has
  those states, as the node has them now, and the model test cuts a
  step there.
- [An ERROR of PostgreSQL in the middle of an action] → the crate calls
  nothing of PostgreSQL, so no jump crosses its frames; the failed
  action ends the spill, and the host frees or resets its state, as
  "When an action fails" says; a machine is plain data in a memory
  context, freed with it, as the node's state is today.
- [A machine too large for one review] → the join's machine comes in
  four tasks and the shared build's in three, each a state or a
  hand-over, each keeping the passes, splits and batches that EXPLAIN
  shows in the suite `join`.
- [The machine of several processes is too large for loom] → models
  of each hand-over apart, as the models of the build and the rounds are
  apart today, each with bounded preemptions.
- [A machine's step costs more than the C it replaces] → steps are per
  chunk or block, counted by the new benchmark, timed by `cargo ab`.
- [A rule made one changes plans] → it moves as parameters first; one
  rule only in a task of its own, with a measurement.
- [The change is long, and `main` moves] → each task is a pull request
  into `main`; nothing waits on a branch.

## Migration Plan

`include/tessera/runtime_spill.h` keeps its calls. The calls of single
decisions in `include/tessera/spill.h`, `include/tessera/table.h` and
the kernels' table give way to the calls of a set and of the machines,
each in the task that moves its user, which edits the specs of those
calls in the same commit. No setting, plan or result
changes, except where a fixed fault says what changes. The format on
disk does not change; a file lives only as long as its query, so no
file of an older build is ever read. Each task is reverted alone.
