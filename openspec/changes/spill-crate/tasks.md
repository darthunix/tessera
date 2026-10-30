## 0. How the work goes

The tasks reach `main` by pull requests, each of a few related tasks, a
task of one to three commits. A task brings its evidence with it: the
model, the property test, the cases of damage and the documentation of
what it adds come in the same task as the code, not later. A task that
moves a file named by a spec or a design edits those paths in the same
commit. A task that replaces a call promised by `hash-table` or
`spill-format` edits that spec in the same commit: it writes the
requirement again for the new call, or moves it, with its tests, into
this change's spec of `relspill`.

Each commit that moves or changes Rust is checked by `make rust-check`,
and by `tessera-bench --base <previous> --disasm` on every program and
with `--module nodes` and `--module kernels`. A loop may grow only by a
check the promise needs, written down with its reason. From task 3.1
on, CI runs Miri over the crate's tests, the mutants of the lines a
pull request changes, and the crate's documentation with every warning
an error. Each task that changes a node runs `make installcheck`,
`make crosscheck` with the spills its generator makes, and a case that
checks that the changed path ran: a split, a piece, a round, a pass of
a merge.

## 1. The proposal

- [x] 1.1 Write the proposal, the spec, the design and the tasks of
      this change. Check: `openspec validate spill-crate --strict`;
      `.github/scripts/check-specs.sh`
- [x] 1.2 Write the crate's contract, `contract.md` in this change's
      folder: the rules of "The crate's contract" in the design, each
      with its check, or `pending` until its task, and the forecast.
      Check: `.github/scripts/check-specs.sh`
- [x] 1.3 Take up the items of the roadmap: the entry `spill-crate` in
      the queue; "A crate for spilling" out of "Decided against"; the
      two findings of `spill-format` out of "Not placed"; and, out of
      `small-c-leftovers`, the row spill, its writer, and the merge of
      the copies of `store_value`. Check:
      `.github/scripts/check-specs.sh`
- [x] 1.4 Fold in the findings of two outside reviews of the proposal,
      and the choices made on them. Check: the review of the pull
      request

## 2. The spill as it is, checked

The tasks go by the harm their fault does: 2.2, 2.7, 2.8, 2.9, 2.10,
2.16, 2.3, 2.14, 2.15, 2.11, 2.12 and 2.13, then 2.4 to 2.6.

- [x] 2.1 Have outside reviews read the spill: the C of the nodes and
      of `runtime/spill.c`; the Rust of `tessera-spill`, of the spill
      words, of the phases, of the appending of columns, and of their
      entry points; and the specs and designs of `spill-format` and
      `hash-table`. Each review is asked for faults that change a
      result, crash, leak, hang or break a promise of a spec. Write the
      accepted findings and their fixes as tasks of this section; each
      is fixed before its code moves. Check: the review of the list in
      the pull request
- [x] 2.2 Show by tests, then fix, an expanded value copied as a
      pointer: in the rows of a grouping that spills, and in the rows
      that the gather of workers sends. One copy of a value by
      reference, in C, which flattens an expanded value, serves every
      writer of rows, its count of bytes included; the copies of the
      join, of its spill and of `runtime/rows.c` become it. Check: a
      test of `agg` with an expanded array argument and rows that
      spill, which checks that the spill ran; a test of `parallel` over
      such a value; a C test that frees the expanded value before its
      rows are read back; `make installcheck`
- [ ] 2.3 Show by tests, then fix, damaged bytes that reach the C of the
      join. It reads any outer block that is not a block of values as
      columns, and a chunk of columns by the side's count of words, not
      the chunk's. It grows its table of value chunks by a number read
      from the file, which overflows a signed `int` at 2^30 and may
      then loop without end and without a check for a cancel. When it
      splits a partition read back, and when it gathers the values of
      a table read back in pieces or in a round, it takes the chunk
      number of a reference without a bound; the gather has only an
      `Assert`. It checks the chunk a reference names but not the byte
      in it, and no check makes sure that a value's length ends inside
      its chunk. After the fix, the join's reads take the kind and the
      shape they expect, a chunk's number is bounded by the chunks
      written, and a value ends inside its chunk. Check: tests of
      `test/tessera_spill_test.c` over damaged files through the
      join's reads, each refused with `XX001`, under the sanitizers of
      CI; `make installcheck`
- [ ] 2.4 Refuse a partition of a shared set that is dropped before the
      set is finished; correct the comment that says a block of columns
      is checked before anything is written. Check: a case of
      `tessera_test_spill_error`; `make installcheck`
- [ ] 2.5 Run the tests of the C entry points of the spill under Miri,
      and fix what it finds. Check: the Miri job of CI
- [ ] 2.6 Add a counter program to `tessera-bench` for the kernels of a
      spill: packing and unpacking a chunk of records and a chunk of
      columns, appending rows by partition, and the header of a block.
      Check: the program runs under `tessera-bench`; a first run of the
      counters, and a run of `runs_text` and `rows_text` on mains
      power, as the base for later comparisons
- [x] 2.7 Show by a test, then fix: a join keeps on disk every key no
      column already holds as a word of 8 bytes, which for a hashed
      key of text, such as an expression `lower(a.t)` its child
      computes, is the address of a value its batch frees; a join that
      spills then reads other bytes, and the server crashes. Such a
      key is kept by its type, and a by-reference one is copied like
      any other column. Check: a test of `join` with an expression key
      over text that spills, in one process, in each participant's
      table and in a shared table, compared with the core;
      `make installcheck`
- [x] 2.8 Show by a test, then fix: the queue of partitions whose
      values outgrew a chunk has one entry a partition, but a partition
      leaves it only when the queue is emptied, though a flush clears
      its mark. So a partition flushed and filled again before the
      queue is emptied is queued again, and the queue runs past its
      array and writes over the memory after it: when a partition read
      back is split, which never empties the queue, and within one
      batch whose long values fill a partition several times, on both
      sides and on a shared join's side of resident outer rows. A join
      of long text values at a small `work_mem` fails with an internal
      error. A partition is queued at most once until the queue is
      emptied, and a split empties it as it writes. Check: tests of
      `join` that split a level with long values and that fill one
      partition several times in a batch, compared with the core;
      `make installcheck` under the sanitizers of CI
- [ ] 2.9 Lift the join's limit of 64 outer columns kept on disk,
      which fails a query that spills with SQLSTATE `54000` though the
      format holds 4096 words a row. The C of the join reads one lane
      of NULL bits and reaches lanes by `1 + word`; the entry point
      `tess_spill_columns_append_partitioned` refuses more than 64
      columns and keeps them in arrays of 64; its kernel writes one
      lane of NULL bits. All of them take the lanes of NULL bits and
      the words of a chunk as `tessera/spill.h` lays them out. Check:
      a Rust test of the entry point with 70 columns, NULLs and a
      by-reference value past column 64; a test of `join` that spills
      with 70 outer columns kept, compared with the core;
      `make installcheck`; the machine code and the counters of the
      kernel's loop, as section 0 asks
- [x] 2.10 Size a partition of a shared build that one participant
      joins alone by the bytes every participant wrote, which the
      shared counters of a partition already hold, and not by its
      records alone, so that a partition of long values is split or
      read in pieces like a serial join's, not loaded whole. How far a
      piece may pass its room by a block or a group is the rule of
      3.23, not of this task. Check: a parallel test of `join` with a
      skewed key of long values, compared with the core, that shows
      the partition taken alone and read in pieces; `make installcheck`
- [ ] 2.11 Check for a cancel in every loop that can run long: the
      linking of a shared table's chunks, a round's load block by
      block (today once a file), the first split of a table, and the
      demotion of a resident partition. Check: a C test of
      `test/tessera_interrupt_test.c` that marks a cancel pending
      before each loop it can reach and shows that the loop stops
      before its end; `make installcheck`
- [ ] 2.12 Take the partitions of the grouping's spilled rows from the
      low bits of the hash, as its spilled records do, and not from
      the high bits, which the buckets of a partition's table take
      too: today most buckets of such a table stay empty and its
      chains grow long. The design's three rules of the bits of a
      level become two. Check: a test of a level's bits that the rows
      of a partition spread over the buckets of its table;
      `cargo ab measure` of a grouping of a generic aggregate over many
      groups at a small `work_mem`, before and after;
      `make installcheck`
- [ ] 2.13 Make the spill words keep their promises: an update they
      refuse leaves them as they were, and
      `tess_table_spill_records` refuses a partition past the level's
      count also when it adds nothing and asks for no count. Correct
      the spec of `hash-table` where it says that a negative partition
      is taken only before the split. Check: Rust tests of both;
      `make rust-check`; `.github/scripts/check-specs.sh`
- [ ] 2.14 Show by tests, then fix, damaged bytes that reach the C of
      the sort: it reads a chunk of columns by its own count of
      columns, not the chunk's; its reader of a single run, which a
      scrollable cursor uses, does not compare a block's rows with the
      run's record of them, and reads past the block when they differ;
      and it checks where a value starts but not that its length ends
      inside its chunk. Check: tests of `test/tessera_spill_test.c`
      over damaged runs, each refused with `XX001`, under the
      sanitizers of CI; `make installcheck`
- [ ] 2.15 Show by tests, then fix, damaged bytes that reach the C of
      the grouping: its reader of spilled rows reads a chunk by its
      own count of words, not the chunk's; its readers of records do
      not check a block's kind; and it checks where a value starts but
      not that its length ends inside its chunk. Check: tests of
      `test/tessera_spill_test.c` over damaged partitions, each
      refused with `XX001`, under the sanitizers of CI;
      `make installcheck`
- [x] 2.16 Show by a test, then fix: a join reads a partition in pieces
      by its groups of records, and a group ends only at a block of
      values when the side has a by-reference column. A partition
      whose by-reference values are all NULL writes no block of
      values, so its piece never ends and the partition that did not
      fit is loaded whole. A piece also ends before a group that has
      no block of values. Check: a test of `join` with a skewed key
      whose text payload is NULL, compared with the core, that shows
      extra passes; `make installcheck`

## 3. The crate

- [ ] 3.1 Make `crates/relspill` from `crates/tessera-spill`, a move
      and a rename only, with a manifest of its own, `publish = true`,
      and a build that refuses 32-bit targets. Check:
      `make rust-check`; the same machine code;
      `cargo package -p relhash -p relspill`
- [ ] 3.2 Make the layout of a record in `relhash` a small public API,
      documented, with its requirement in the spec of `relhash` and in
      its snapshot of the public API. Check: the diff of the snapshot;
      `make rust-check`; the review of the new API in the pull request
- [ ] 3.3 Make the packing of records take the layout from `relhash`;
      the bare numbers go. Check: tests that pack and unpack records
      that a table of `relhash` wrote; the same machine code
- [ ] 3.4 Move `shared_spill.rs`, `phases.rs`, their loom models and
      `spill_columns.rs` from the kernels to the crate. It is a plain
      move, except for the words in shared memory: the crate takes them
      as a slice of atomic words that `tessera-capi` makes. The kernels
      export the moved names again. Check: `cargo ab moved`; the same
      machine code; `make rust-loom`; `make installcheck`
- [ ] 3.5 Replace the crate's errors of text with errors of two kinds in
      one word, as `relhash` has them; a limit of the host stays the
      host's error. Check: `make rust-check`; the tests of SQLSTATE in
      the suite `spill`; the machine code
- [ ] 3.6 Move the writing of a set into the crate, without input or
      output: the write buffer, the block lists, the trailer, and what
      a set accepts after a failed write. `runtime/spill.c` keeps its
      calls of PostgreSQL and its C API. The regions come from the
      caller; the block lists stay outside the measure of memory, as
      today. Check: a property test of a set in memory, with writes cut
      by failures; `make installcheck`; `cargo ab measure` of the
      family `spill`
- [ ] 3.7 Move the reading of a set into the crate: the set's own lists,
      and another participant's trailer and lists, a page of entries at
      a time, from the length of the file that the host gives; each is
      checked before the bytes it names are asked for. Check: the cases
      of damaged lists and trailers; a file written before the move and
      read after it; `make installcheck`
- [ ] 3.8 Make every node pass the bounds of its blocks, a chunk by the
      plan of its level and a block of values by its largest row,
      instead of the largest allocation PostgreSQL allows; a reader of
      a shared set takes the bounds its writers agreed on. Check: tests
      of wide values and of long lists; `make installcheck`
- [ ] 3.9 Make the level plan one rule: the partitions, the chunks and
      the words of a level, the expected bytes of level 0, the shift of
      a level below, and the bits of the hash each level takes, with
      the differences of each node as parameters. The join, the shared
      join, the grouping and the estimate in the grouping's planner use
      it. Check: `make installcheck`; the machine code
- [ ] 3.10 Make one writer and one reader of a chunk of columns, and
      move the grouping's rows to them; the copy of 2.2 stays before
      the writer. Check: `make installcheck`; `cargo ab measure` of
      `rows_text` and of the family `spill`
- [ ] 3.11 Move the sort's runs to the writer and the reader of 3.10.
      Check: `make installcheck`; `cargo ab measure` of `runs_text`
- [ ] 3.12 Move the join's outer rows and the gather of workers to the
      writer and the reader of 3.10. Check: `make installcheck`; the
      parallel tests of the suite `join`; `cargo ab measure` of the
      cases of `join` that spill
- [ ] 3.13 Write the machine of the join's spill, its build: the spill
      at level 0 and the partitions sent to disk; the node still
      decides the rest. An outer row keeps a hashed key on disk as the
      64-bit hash the table keys by, a word of 8 bytes, not as its
      value: no copy of the value, and no second hashing of a batch
      read back, which the join tells from its child's. Check: the
      machine's property test against a join in memory; the tests of
      2.7; `make installcheck`; `make crosscheck`; `cargo ab measure`
      of the family `spill`
- [ ] 3.14 Add to the join's machine the partitions on disk, joined
      whole or split into a level below. Check: the property test; the
      cases of nested levels; `make installcheck`; `make crosscheck`
- [ ] 3.15 Add the pieces and their passes, with the marks of matched
      rows. Check: the property test; the cases of pieces, of skew and
      of a partition without values (2.16); `make installcheck`;
      `cargo ab measure` of a join read in pieces
- [ ] 3.16 Add the tails of RIGHT and FULL joins and the output held
      across calls; the decisions of C go. Check: the property test;
      the cases of tails and of early stops; `make installcheck`;
      `make crosscheck`
- [ ] 3.17 Write the machine of the grouping's spill, for its groups,
      and make the node step it. Check: the property test against a
      grouping in memory; `make installcheck`; `cargo ab measure` of
      the family `spill`
- [ ] 3.18 Add the rows of generic aggregates to the grouping's
      machine, composed with its groups. Check: the property test; the
      cases of both spills and of keys numbered by a dictionary;
      `make installcheck`
- [ ] 3.19 Write the machine of the sort's runs, with a rescan and a
      scan backward, and make the node step it. Check: the property
      test against a sort in memory; `make installcheck`;
      `cargo ab measure` of `runs_text` and of sorts that merge in
      several passes
- [ ] 3.20 Write the machine of a build by several processes: its
      phases from `phases.rs`, with what C decides about writing to
      disk. Check: the loom models, old and new; `make rust-loom`; the
      parallel tests of the suite `join`; `cargo ab measure` of a
      shared join without a spill
- [ ] 3.21 Add the agreement on a split, the publishing of the filter
      before it, and the catch-up of a participant that attaches late.
      Check: a loom model of each; `make rust-loom`; the parallel tests
      of the suite `join`
- [ ] 3.22 Add the rounds, the partitions taken alone, which go to the
      join's machine, and the frees. Check: loom models of the
      hand-overs; `make rust-loom`; `make crosscheck` with workers;
      `cargo ab measure` of the rounds of a parallel join
- [ ] 3.23 Make the measure of memory count the block lists, the marks
      of matched rows of a join read in pieces, the block it reads
      ahead, and the writers and buffers of the grouping's spill of
      rows, which only grow, each by its capacity and once, with a
      rule of what may go past the limit and of what a node does when
      memory runs out; the specs that show a changed plan change with
      it. Check: `make installcheck`, with every changed plan read;
      `cargo ab measure` of the family `spill`
- [ ] 3.24 Make one rule for the limit of a level below, for the join
      and the grouping, if a measurement shows no loss. Check:
      `cargo ab measure` of the family `spill`; `make installcheck`
- [ ] 3.25 Review the whole public API and settle it, and keep its
      snapshot diffed in CI, as for `relhash`. Check:
      `make rust-check`; the diff of the snapshot; the review of the
      API in the pull request
- [ ] 3.26 Have outside reviews read the crate, as in 2.1, and fix the
      accepted findings. Check: the review of the list in the pull
      request

## 4. The spec of the crate

- [ ] 4.1 Group the requirements of `relspill`, the moved ones and this
      change's, by topic; check that the specs of `hash-table` and
      `spill-format` name no call that is gone. Check:
      `openspec validate --all --strict`;
      `.github/scripts/check-specs.sh`

## 5. The documentation

- [ ] 5.1 Write the README as the crate's front page, with an example
      of a spill over files kept in memory. Check:
      `cargo test -p relspill --doc`
- [ ] 5.2 Read the documentation whole, as on docs.rs, and add what a
      caller who reads only it would miss: the actions and their
      failures, the order of memory at a barrier, the bounds of a read.
      Check: `cargo doc -p relspill` with every warning an error; the
      review of the pull request

## 6. The checks of the box

- [ ] 6.1 Mark every read of bytes from a file as a trust point, list
      the trust points in the design of `relspill`, and make the script
      of trust points check the crate, failing also on a read that goes
      around the layer. Check: the script in CI
- [ ] 6.2 Kani proofs, one family a pull request: the checks of a
      header and a trailer; the lists; the bounds of unpacking records;
      those of columns; the arithmetic of a level and of the rule that
      picks a partition to send to disk. Each family names, in the list
      of trust points, the checks it covers. Check:
      `cargo kani -p relspill` in CI
- [ ] 6.3 The damage matrix: damaged files against every read of the
      crate and of the nodes. Check: its tests
- [ ] 6.4 Fuzz targets: any bytes as a file; any events and numbers
      given to a machine. Check: the fuzz workflow, a minute a target
      on a pull request, and an hour a target before the close
- [ ] 6.5 Tests that no call of the crate allocates memory, under a
      counting allocator. Check: `make rust-check`
- [ ] 6.6 The checks of the contract, for every rule still pending: the
      crate packaged with `relhash` alone; built as `no_std` without
      its feature `alloc`, so that a file or an allocation fails to
      build; `#![forbid(unsafe_code)]` checked in CI; the script of
      trust points; the snapshot of the public API. Check:
      `.github/scripts/check-specs.sh` with no rule pending
- [ ] 6.7 Every mutant of the crate caught by the crate's own tests, or
      listed in `.cargo/mutants.toml` with its reason. Check: a run of
      the mutants workflow for `relspill`
- [ ] 6.8 Probes of the contract's forecast, on branches that are never
      merged, with their counts in the design. Check: the review of the
      counts in the pull request

## 7. The close

- [ ] 7.1 Write the design of `relspill` for a caller of the crate.
      Check: `.github/scripts/check-specs.sh`
- [ ] 7.2 Run the fuzz targets for an hour each. Check: the run, with
      no finding
- [ ] 7.3 Archive the change: the contract beside the spec, the
      roadmap updated, the folder deleted. Check:
      `openspec validate --all --strict`;
      `.github/scripts/check-specs.sh`
- [ ] 7.4 Give the outcome in the pull request that closes the change.
      Check: the outcome is approved
