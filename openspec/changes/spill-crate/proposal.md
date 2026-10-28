## Why

A spill writes the rows of a hash join, a grouping or a sort to
temporary files when they do not fit in memory, and reads them back.
It is where wrong results and crashes hide best: it runs only under
memory pressure, its paths multiply with levels, pieces and processes,
and several past faults of Tessera's joins came from it. On 2026-10-28
the maintainer decided to make the spill a crate of its own, `relspill`,
as `relhash` was made of the hash table: free of Tessera, checked as
fully as the tools allow, and no slower. Today it is far from that:

- The protocol of a spill lives in about 7,200 lines of C:
  `nodes/hashjoin_spill.c` (2,344), `nodes/hashjoin_shared.c` (1,698),
  `nodes/agg_spill.c` (1,371), `nodes/sort_external.c` (878) and
  `runtime/spill.c` (945). Rust decides four things in it: how many
  partitions a level has, how long a chunk is, which partition goes to
  disk, and whether a partition read back splits again. Every other
  decision is C control flow, mixed with writes, reads and the
  copying of values in the same functions.
- The same rule is written in several places: the loop that sends
  partitions to disk three times, the making of a level three times,
  the writer and the reader of a chunk of columns five times, the
  partition of a hash in C and in Rust. Two copies have already drifted
  apart. The join decides whether a partition splits by the setting
  `tessera.join_spill_split_room`, and whether it is read in pieces by
  two thirds written in the code: with the setting changed, the two
  thresholds part. The join gives a level below a smaller limit of
  memory; the grouping gives every level the whole of `hash_mem`.
- The format of a file is in two places. Blocks are written by
  `tessera-spill` in Rust; the lists of blocks and the trailer of a
  file that several processes read are written and checked by C alone.
- The checks are thin. `tessera-spill` has 20 unit tests and no
  property test, proof or fuzzer; 21 of its mutants survive, 15 of them
  in the packing of records. Miri runs none of the C entry points of
  the spill, whose unsafe code turns pointers from C into slices and
  reads PostgreSQL's values. The loom models leave out the files, the
  hand-over from a shared build to its rounds, the catch-up of a
  process that joins late, and the publication of the filter before
  the split. Faults fixed so far were of this kind: a late participant
  that zeroed the shared values, a loop that sent the same partition to
  disk forever, a RIGHT join whose last participant missed one that had
  stopped.
- The packing of records copies `relhash`'s layout of a record as bare
  numbers, so a change of that layout would break the spill without an
  error from the compiler.
- Memory goes uncounted: the lists of blocks, the matched rows of a
  join read in pieces, the block a join reads ahead. Every node passes
  the largest allocation PostgreSQL allows as the longest block, so a
  damaged list can ask for a buffer as large as the file.
- No counter benchmark runs the kernels of a spill, so their
  instructions are not counted.

## What Changes

- A new crate, `relspill`, holds the spill: the format of a block and
  of a file, the plan of a level, the words of a spill that several
  processes share, and the protocol, as state machines, of the join's
  spill, the grouping's spill, the sort's runs, and a build by several
  processes with its rounds over partitions on disk. It depends on
  `relhash` alone, builds only for 64-bit targets, and is marked
  `publish = true`, so its public API is a surface of a spec.
- The crate does no input or output. It keeps the write buffer, the
  lists of blocks and the trailer, and tells its caller which bytes to
  write at which offset and which to read; it checks the bytes the
  caller reads back. Calls of PostgreSQL's files stay in
  `runtime/spill.c`. A machine of the protocol is a value the caller
  steps with events, such as "a batch went in" or "a block was read",
  and each step returns the next action: write a block, read one, wait
  for the others, give out rows. The rows themselves, their keys and
  their values stay in the kernels and in `relhash`.
- A call of the crate allocates no memory. Each region it uses comes
  from the caller, and a call that needs more says how much, so the
  memory of a spill is counted where PostgreSQL counts memory.
- Tessera keeps what belongs to SQL and to PostgreSQL: the values by
  reference and their copying, the files and their limits, barriers,
  shared memory, memory contexts, interrupts, errors and their
  SQLSTATE, settings and EXPLAIN. The nodes drive the machines; the
  C that decided goes.
- Each rule of a spill gets one owner, the crate. A step that moves a
  rule keeps what SQL sees; where the copies differ, a task of its own
  chooses one rule, with a measurement.
- The spill is validated before it moves. Outside reviews read the
  spill as it is, in C and in Rust, and their findings are fixed, each
  with a test, before the code moves. The faults found while this
  proposal was written are among the first: the two thresholds of the
  join, and the copying of the grouping's rows, which handles no
  expanded value while the join's copying does.
- An error of the crate is of one of two kinds, a wrong call or
  damaged bytes, and names the rule it broke, as `relhash`'s errors do.
- A read of a file is bounded: any bytes give an error, never a crash,
  an endless loop or a buffer larger than the node's longest block.
  Every node passes its longest block. The promise is shown by a list
  of the places where the crate trusts bytes it reads, Kani proofs of
  its checks, a matrix of damaged files against every read, fuzzing,
  mutants of the whole crate down to none that survive, and Miri over
  all its tests and over the C entry points of the spill.
- The machines are tested against a model: a join, a grouping and a
  sort in memory, without a spill, give the same rows as the machines
  under every limit of memory a property test chooses, over files kept
  in memory. The machine of several processes runs under loom with its
  files, its catch-up and its hand-over to the rounds.
- A counter benchmark of `tessera-bench` runs the kernels of a spill:
  packing and unpacking records and columns, appending by partition.
- The requirements of `spill-format` that describe the format, and
  those of `hash-table` that describe the words of a spill, the levels
  and the rounds, move to the capability `relspill` in changes of
  their own, once the code is in the crate. This change adds the new
  promises.
- The crate gets an architecture contract: free of Tessera and of SQL,
  no input or output, no memory taken, bytes from disk checked by the
  crate alone, unsafe code only where it is proven, one owner for each
  rule of a spill, a small surface, and no cost to Tessera.
- The work goes as pull requests into `main`, one a task, each read by
  the maintainer.

## Capabilities

### New Capabilities
- `relspill`: the crate of its own, no input or output, no memory
  taken, the file of a set, the spill of a join, the spill of a
  grouping, the runs of a sort, a build by several processes and its
  rounds, any bytes and a bounded read, and errors of two kinds. The
  requirements of the format and of the words of a spill join it from
  `spill-format` and `hash-table` in changes of their own.

### Modified Capabilities

None in this change's deltas. `spill-format` keeps the C API of the
sets and what SQL sees of a spill; the changes that move its format and
`hash-table`'s spill to `relspill` modify both.

## Impact

- Code: a new `crates/relspill` from `crates/tessera-spill`, which it
  replaces, with `shared_spill.rs`, `phases.rs`, the loom models of the
  spill and the rounds, and the appending of rows to chunks of columns
  from `tessera-kernels`. `runtime/spill.c` keeps PostgreSQL's file
  calls and gives the rest to the crate. The spill parts of
  `nodes/hashjoin_spill.c`, `nodes/hashjoin_shared.c`,
  `nodes/agg_spill.c` and `nodes/sort_external.c` shrink to driving
  the machines; the grouping's rows and the sort's runs share one
  writer and one reader of a chunk of columns.
- C API: `include/tessera/runtime_spill.h` keeps its calls.
  `include/tessera/spill.h`, the spill calls of
  `include/tessera/table.h` and the kernels' table
  `include/tessera/kernel_ops_list.h` change: the calls of a set and of
  the machines replace the calls of single decisions. `spill-format`
  and `hash-table` say so in the changes that move their requirements.
- SQL: no result, plan or setting changes, but where a fault the review
  finds is fixed; each such fix names what SQL sees differently.
- SQL suites: `spill`, `join`, `agg`, `sort` and `parallel` keep their
  outputs; tests are added where a fix needs one.
- Tests: new Kani proofs, a matrix of damaged files, fuzz targets,
  property tests of the machines against models, loom models with
  files, counter benchmarks; Miri over the C entry points of the spill.
- Roadmap: takes up "A crate for spilling", which the close of
  `table-crate` left under "Decided against" by mistake; the two
  findings of `spill-format` under "Not placed"; and the row spill and
  its writer from `small-c-leftovers`.
