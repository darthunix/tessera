## Why

When a hash join, a grouping or a sort gets more rows than fit in its
memory, it writes some of them to temporary files and reads them back
later. This is called a spill. Faults hide well in a spill:

- it runs only when memory is short, so most tests never reach it;
- its paths multiply: a partition on disk may be split again, read in
  pieces, or shared by several processes;
- several past faults of Tessera's joins were in it.

This change makes the spill a Rust crate of its own, `relspill`, the
way the hash table became the crate `relhash`. The crate will not
depend on Tessera, will be checked as fully as our tools allow, and
will be no slower. Today the spill is far from that.

- **Most of it is C.** About 7,200 lines of C run a spill:
  `nodes/hashjoin_spill.c` (2,344 lines), `nodes/hashjoin_shared.c`
  (1,698), `nodes/agg_spill.c` (1,371), `nodes/sort_external.c` (878)
  and `runtime/spill.c` (945). Rust makes only a few decisions: how
  many partitions a level has, how long a chunk is, which partition
  goes to disk, and whether a partition read back is split again. It
  also moves a process of a shared build through its phases and
  rounds. Every other decision is C, mixed in the same functions with
  writes, reads and copies of values.
- **Rules are written more than once.** The loop that sends partitions
  to disk exists three times, and so does the code that sets up a
  level. The code that writes and reads rows stored by column exists
  five times. The partition of a hash is computed in C and in Rust.
  The point where a grouping starts to spill is computed in the node
  and again in its planner. Two copies already differ: a join gives a
  level below a smaller memory limit, while a grouping gives every
  level all of `hash_mem`.
- **The file format lives in two places.** Rust (`tessera-spill`)
  writes the blocks. C alone writes and checks the block lists and the
  trailer of a file that other processes read.
- **The checks are thin.** `tessera-spill` has 20 unit tests, and no
  property test, proof or fuzzer. 21 of its mutants survive, 15 of
  them in the code that packs records. Miri runs the tests that append,
  pack and unpack column chunks, and the tests of the phases. It does
  not run the other C entry points of the spill, whose unsafe code
  reads PostgreSQL's values by reference. The loom models treat a file
  as a list of keys. They leave out the file's bytes, the hand-over
  from a shared build to its rounds, the catch-up of a process that
  joins late, and the publishing of the filter before a split. The
  faults fixed so far were of just this kind: a late process that
  zeroed the shared values, a loop that sent the same partition to
  disk forever, and a RIGHT join whose last process missed one that
  had stopped.
- **Damaged bytes reach C code that trusts them.** The sort reads a
  column chunk with its own count of columns, not the chunk's, so a
  damaged chunk makes it read past its memory. The join grows its
  table of value chunks by a number read from the file; at 2^30 this
  overflows an `int`. Two outside reviews of this proposal found both.
- **Nothing says what happens when a file operation fails half way.**
  PostgreSQL ends the query, but another host is not told what it must
  do.
- **The record layout is copied as plain numbers.** The code that packs
  records repeats `relhash`'s record layout as bare offsets. If
  `relhash` changed the layout, the spill would break with no error
  from the compiler.
- **Some memory is not counted.** Nobody counts the block lists, the
  marks of the outer rows that matched in a join read in pieces, or
  the block a join reads ahead. Every node passes PostgreSQL's largest
  allowed allocation as the size of its longest block, so a damaged
  list can ask for a buffer as large as the file. A partition's block
  list is read whole, however long it is.
- **No counter benchmark covers the spill,** so nobody counts the
  instructions of its kernels.

## What Changes

- **A new crate, `relspill`.** It holds the format of a block and of a
  file; the level plan, which says how many partitions a level has and
  how long its chunks are; the spill words, which track the size and
  the flags of each partition and the memory budget, and which several
  processes may share; and the protocol of each spill as a state
  machine: the join's spill, the grouping's spill, the sort's runs, and
  a hash table built by several processes, with its rounds over the
  partitions on disk. It depends on `relhash` alone and builds only for
  64-bit targets. It is marked `publish = true`, so a spec describes
  its public API.
- **No file input or output in the crate.** The crate never touches a
  file. It keeps the write buffer, the block lists and the trailer. It
  tells its caller which bytes to write at which offset, and which
  range to read, and it checks the bytes the caller reads back. The
  calls to PostgreSQL's files stay in `runtime/spill.c`. A state
  machine is a value the caller steps with events, such as "a batch
  went in" or "a block was read". Each step returns the next action:
  write a block, read one, wait for the other processes, give out
  rows. The rows, their keys and their values stay in the kernels and
  in `relhash`.
- **No memory allocated by the crate.** Every region a call uses comes
  from the caller. A call that needs more says how much. So the memory
  of a spill is counted where PostgreSQL counts memory.
- **A failed action ends the spill.** If the host cannot finish a write
  or a read, the crate does not retry it. The set or the machine then
  accepts only a free or a reset. The design says what a host must
  give: which failures are its own and which mean damaged bytes, the
  length of a file, and the memory order its barrier keeps.
- **Tessera keeps what belongs to SQL and to PostgreSQL:** values by
  reference and their copying, the files and their limits, barriers,
  shared memory, memory contexts, interrupts, errors and their
  SQLSTATE, settings and EXPLAIN. The nodes drive the machines, and
  the C that made the decisions goes away.
- **One owner for each rule of a spill: the crate.** A step that moves
  a rule keeps everything SQL sees. Where two copies of a rule differ,
  a task of its own picks one rule, with a measurement. The join's two
  thresholds are not such copies. `tessera.join_spill_split_room`
  decides whether a partition read back is split; two thirds of the
  free room decide how large a piece is. They stay two rules.
- **The spill is checked before it moves.** Outside reviews (other AI
  models, given the code and asked for faults) read the spill as it is
  today, in C and in Rust. Each finding is fixed, with a test, before
  its code moves. The faults already found come first:
  - the grouping's rows copy an expanded value as a pointer, and the
    gather of parallel workers may do the same;
  - damaged bytes reach the sort and the join;
  - a partition of a shared set can be dropped before the set is
    finished.
- **Errors of two kinds.** An error of the crate is either a wrong call
  or damaged bytes, and it names the rule that was broken, as in
  `relhash`.
- **Every read is bounded.** Whatever bytes a file holds, a read gives
  the data or an error. It never crashes, never loops without end, and
  never asks for more than its caller allows: one block of the kind it
  reads, or one page of a list. Every node passes the real bounds of
  its blocks. The promise is shown by:
  - a list of every place where the crate trusts bytes from a file;
  - Kani proofs of its checks;
  - a damage matrix: files with each field damaged, against every read
    of the crate and of the nodes;
  - fuzzing;
  - mutation testing of the whole crate, until no mutant survives;
  - Miri over all the crate's tests and over the C entry points of the
    spill.
- **The machines are tested against a model.** A plain join, grouping
  or sort in memory, without a spill, must give the same rows as the
  machine, under any memory limit a property test picks, with files
  kept in memory and actions that fail. The machine of several
  processes runs under loom, with its files, its catch-up and its
  hand-over to the rounds. Each machine comes with its model, in the
  task that writes it.
- **A counter benchmark for the spill.** A new program of
  `tessera-bench` counts the instructions of packing and unpacking
  records and columns, of appending rows by partition, and of the
  steps of each machine.
- **The specs move with the code.** The requirements of `spill-format`
  about the format, and those of `hash-table` about the spill words,
  the levels and the rounds, change in the task that changes their
  calls, and move to `relspill` there. This change adds the new
  promises.
- **`relhash` makes its record layout public**, as a small API, so that
  the spill packs records by it and not by a copy.
- **The crate gets an architecture contract:** free of Tessera and of
  SQL; no file input or output; no memory allocated; bytes from disk
  checked in one place; no unsafe code; one owner for each rule of a
  spill; a small public API; and no cost to Tessera.
- **About fifty tasks,** each one pull request into `main`.

## Capabilities

### New Capabilities

- `relspill`: the crate of its own; no file input or output; no
  memory allocated; the file of a set; the spill of a join; the spill
  of a grouping; the runs of a sort; a build by several processes and
  its rounds; any bytes, a bounded read; errors of two kinds. The
  requirements of the format and of the spill words join it from
  `spill-format` and `hash-table`, in the tasks that change their
  calls.

### Modified Capabilities

- `hash-table`: the calls that make single decisions of a spill, and
  the phases and the rounds, give way to the machines. Each task that
  replaces such a call edits this spec in the same commit. The
  requirements on the spill words, the levels, the phases and the
  rounds move to `relspill`.
- `spill-format`: the requirements on the format move to `relspill` in
  the same way. The spec keeps the C API of a set and what SQL sees of
  a spill.
- `relhash`: the record layout becomes public.

## Impact

- **Code:** a new crate, `crates/relspill`, replaces
  `crates/tessera-spill`. It also takes from `tessera-kernels`
  `shared_spill.rs`, `phases.rs`, the loom models of the spill and the
  rounds, and the appending of rows to column chunks.
  `runtime/spill.c` keeps PostgreSQL's file calls and gives the rest
  to the crate. The spill code of `nodes/hashjoin_spill.c`,
  `nodes/hashjoin_shared.c`, `nodes/agg_spill.c` and
  `nodes/sort_external.c` shrinks to driving the machines. The
  grouping's rows and the sort's runs share one writer and one reader
  of column chunks.
- **C API:** `include/tessera/runtime_spill.h` keeps its calls. The
  calls of single decisions in `include/tessera/spill.h`, the spill
  calls of `include/tessera/table.h` and the kernels' table
  `include/tessera/kernel_ops_list.h` give way to the calls of a set
  and of the machines. `spill-format` and `hash-table` say so in the
  task that changes each call.
- **`relhash`:** a small public API for the record layout.
- **SQL:** no result, plan or setting changes, except where a fixed
  fault, or memory counted for the first time, changes one. Each such
  task says what SQL sees differently.
- **SQL suites:** `spill`, `join`, `agg`, `sort` and `parallel` keep
  their outputs; a fix adds tests where it needs them.
- **Tests:** new Kani proofs, a damage matrix, fuzz targets, property
  tests of the machines against models, loom models with files,
  counter benchmarks, and Miri over the C entry points of the spill.
- **Roadmap:** the change takes up "A crate for spilling", which the
  close of `table-crate` put under "Decided against" by mistake; the
  two findings of `spill-format` under "Not placed"; and, from
  `small-c-leftovers`, the row spill, its writer, and the merge of the
  copies of `store_value`.
