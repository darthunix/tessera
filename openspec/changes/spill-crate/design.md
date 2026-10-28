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
  reads PostgreSQL's values by reference, with unsafe code that Miri
  never runs.
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
  PostgreSQL's values; the copying of values by reference in the nodes;
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
  it asks for the trailer at the end of the file, then the counts, then
  the entries of one partition, and checks each before it asks for the
  next: the same checks C does today, now in one place with the
  header's. The host reads each range and hands the bytes back.
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
that needs more returns how much, and the caller gives a larger region
or fails. So the memory of a spill is allocated where PostgreSQL counts
memory, and a machine can ask its host for the measure it decides by.

This also makes the memory that nobody counts today visible: the lists
of blocks, the matched rows of a join read in pieces, and the block a
join reads ahead past a piece. Counting them changes when a join sends
partitions to disk, which an earlier attempt showed in five plans of
the suite `join`; it needs a reserve in the rule first. So the step
that moves the lists keeps them out of the measure, as today, and a
task of its own counts them, with the reserve and a measurement.

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
  bits of the hash; the move keeps both.
- **The sort's runs.** When a run goes to disk, the sets of runs, the
  plan of blocks and of the merge's fan-in, and the passes of the
  merge. The merge of the items itself stays in the kernels: the items
  are Tessera's encoding of SQL's order.
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
rounds would otherwise sit on the boundary of the crate.

- **Refused: the decisions as separate calls**, as today. That keeps
  the order of the decisions, their state and their mistakes in C,
  where no model reaches them.
- **Refused: one machine for every operator.** The protocols differ in
  their states; what they share is the plan of a level, the words and
  the file of a set, which the machines share as parts.
- **Refused: async Rust.** A future is a state machine the compiler
  writes, hard to keep in shared memory, to bound, or to prove.

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
- The writer and the reader of a chunk of columns: the grouping's rows,
  the sort's runs twice, the join's outer rows, and the gather of
  parallel workers.
- Which partitions to open, and the routing of outer rows: the join and
  the shared join.
- The start of the grouping's spill at seven eighths of `hash_mem`:
  three places of the node and a setting.

Two copies already differ:

- The join decides a split by `tessera.join_spill_split_room` and
  reading in pieces by two thirds in the code. They agree only at the
  setting's default.
- The join gives a level below what is left of its limit, or a quarter
  of it; the grouping gives every level the whole of `hash_mem`.

A step that moves a rule keeps what SQL sees: where copies differ, the
machine takes each node's rule as a parameter. The first difference is
a fault: a task of the validation fixes it with a test. The second is a
choice: a task of its own makes one rule, with a measurement.

### The spill validated first

Before any code moves, the spill as it is is read by outside reviews,
as `relhash` was: models of other makers, given the code of the spill
in C and in Rust, its specs and its designs, and asked for faults that
change a result, crash, leak or hang. Each finding the maintainer
accepts becomes a task, fixed with a test that fails before the fix.
Moving code with its faults would make them harder to find: the move
keeps behavior, so a fault would move with it.

The survey for this proposal found some already, to be shown by tests
first:

- The two thresholds of the join above.
- The grouping's rows copy a value by reference by its size alone; the
  join flattens an expanded value first. An expanded value in the rows
  of a grouping that spills may be copied as a pointer to memory that
  is freed.
- A shared set dropped before it is finished leaves the partition out
  of its trailer, and nothing guards this.
- A block of columns may flush the buffer to disk before its header is
  checked, though a comment says otherwise.
- The memory nobody counts, above.

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

### Any bytes, a bounded read

The promise: whatever bytes a file holds, a read of the crate returns
an error or the data, never a crash, a loop without end, or a request
for a buffer larger than the longest block the node said it writes. It
holds for a file nobody writes while it is read: a participant reads
another's file only after the barrier that ends its writing.

Every node passes its longest block instead of the largest allocation
PostgreSQL allows, so the bound holds.

The promise is shown as `relhash`'s is:

1. **The trust points.** Every read of bytes from a file goes through
   one layer and carries a mark; a script fails on a read without one.
   The list says for each what the bytes decide (an offset, a length,
   a count of a loop) and which check, error, proof and row of the
   matrix covers it.
2. **Kani.** Proofs for all values of the checks of a header, a
   trailer, the lists, and the bounds of unpacking records and columns,
   and of the arithmetic of a level's plan and of the rule of eviction.
3. **The matrix of damage.** Files written by the crate, with every
   field of a header, a trailer, a list and a packed body damaged at
   and around its limits, against every read. A test checks the rule of
   the error.
4. **Fuzzing.** Any bytes as a file, and any sequence of events and
   numbers given to a machine, which must answer with an action or a
   wrong call's error, never a panic.
5. **Mutants.** Every mutant of the crate caught by its own tests, or
   listed with the reason it is equivalent.
6. **Miri** over all the crate's tests and over the tests of the C
   entry points of the spill.

### The machines against a model

A property test runs each machine over a host of its own: files as
vectors, memory as a count, `relhash`'s owning table. It chooses the
rows, their keys and duplicates, the limit of memory, and the moments a
step is cut by an error. The same rows go through a plain operator in
memory: a join, a grouping, a sort with no spill. The two must give the
same rows, the files must be empty at the end, and the memory a machine
asks for must stay under its limit plus what the rule allows.

The machine of several processes runs under loom with two and three
participants: its phases, its words, its files in memory, its catch-up,
its hand-over to the rounds, its stop and its frees. The models of
today keep running; the new ones cover what they leave out.

### The spec of the crate, in three changes

As with `relhash`, two changes move requirements to the capability
`relspill`, as the code has them, once the code is in the crate:

- one from `spill-format`: the header of a block, its kinds, the
  packed records, the chunk of columns in memory and on disk, the
  reference to a value, their limits, and the lists and trailer of a
  file;
- one from `hash-table`: the counters and phases of a shared build,
  the rounds, the words of a spill, sending partitions to disk, and
  the partitions of a level.

`spill-format` keeps the C API of a set and what SQL sees: the errors
and their SQLSTATE, the limit of temporary files, the tablespaces, a
file removed after an error. This change adds the new promises on top,
and the requirements are then joined by topic.

### The documentation of the crate

As `relhash`'s: complete by itself, every public item documented, the
rules, layouts and limits of the API in it, the README as the crate's
page, built without warnings and its examples run in CI. The spec's
requirements link to the items.

### The contract of the crate

The crate is a new part, so it gets a contract, in this change's folder
until the close. Its rules keep it free of Tessera and of SQL, free of
input and output, free of allocation, its reads of bytes behind one
layer, its unsafe code where it is proven, each rule of a spill in it
alone, its surface small and its cost to Tessera nil. Most are
`pending` until the task that writes their check. Its forecast names
three likely changes: a spill for a merge join, a host with another
manager of temporary files, and a new kind of block. A probe of each
runs once the crate exists.

### Speed

Every commit is one of two sorts, as in `table-crate`.

- **A move.** The machine code of every benchmark program and of the
  kernels' module is the same as the base's: `tessera-bench --base REF
  --disasm` and `--disasm --module`, with `cargo ab moved` for a file
  that is cut.
- **A change.** First the machine code, where no loop may grow without
  a reason written down; then the counters, run by the maintainer, on
  the new benchmark of the spill's kernels; then `cargo ab measure` on
  the spill family of `bench/pg` and on the cases `runs_text` and
  `rows_text` of the exec family.

A machine steps once for a chunk, a block or a partition, not for a
row: its cost is counted by the benchmark, not assumed.

### How the work goes

Each task is a pull request into `main`, read by the maintainer. A
task that moves a file named by a spec or a design edits those paths in
the same commit.

## Measurement plan

- **What:** the machine code and the instructions of the spill's
  kernels and of the machines' steps, and the time of SQL queries that
  spill.
- **Tools:** `tessera-bench --base REF --disasm` on every program and
  with `--module`, every commit; a new program of `tessera-bench` with
  counters for packing and unpacking records and columns, appending by
  partition, and the steps of a machine; `cargo ab measure` on the
  family `spill` of `bench/pg` (joins and groupings at a `work_mem` of
  4, 16 and 64 MB), the cases `runs_text` and `rows_text` of `exec`,
  and the cases of `join` that spill.
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
- **A late participant joining the rounds.** Refused before: a rare
  path no test reaches.
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
  nothing of PostgreSQL, so no jump crosses its frames; a machine is
  plain data in a memory context, freed with it, as the node's state is
  today.
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
each in the task that moves its user. No setting, plan or result
changes, except where a fixed fault says what changes. The format on
disk does not change; a file lives only as long as its query, so no
file of an older build is ever read. Each task is reverted alone.
