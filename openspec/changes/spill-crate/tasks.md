## 0. How the work goes

Every task below is a pull request into `main` of one to three commits,
read by the maintainer. A task brings its evidence with it: the model,
the property test, the cases of damage and the documentation of what
it adds come in the task that adds the code, not after. A task that
moves a file named by a spec or a design edits those paths in the same
commit; a task that replaces a call promised by `hash-table` or
`spill-format` edits that spec in the same commit, writing the
requirement again for the new call or moving it, with its tests, into
this change's spec of `relspill`.

Each commit that moves or changes Rust is checked by `make rust-check`
and by `tessera-bench --base <previous> --disasm` on every program and
with `--module nodes` and `--module kernels`; a loop may grow only by a
check the promise needs, written down with its reason. From 3.1 on, CI
runs Miri over the crate's tests, its mutants of a pull request's
lines, and its documentation with every warning an error. Each task
that changes a node runs `make installcheck`, `make crosscheck` with
the spills its generator makes, and a case that asserts the path it
changes ran: a split, a piece, a round, a merge pass.

## 1. The proposal

- [ ] 1.1 Write the proposal, the spec, the design and the tasks of
      this change. Check: `openspec validate spill-crate --strict`;
      `.github/scripts/check-specs.sh`
- [ ] 1.2 Write the crate's contract, `contract.md` in this change's
      folder: the rules of "The contract of the crate", each with its
      check or `pending` until its task, and the forecast. Check:
      `.github/scripts/check-specs.sh`
- [ ] 1.3 Take up the roadmap's items: the entry `spill-crate` in the
      queue; "A crate for spilling" out of "Decided against"; the two
      findings of `spill-format` out of "Not placed"; the row spill and
      its writer, and the merge of the copies of `store_value`, out of
      `small-c-leftovers`. Check: `.github/scripts/check-specs.sh`
- [ ] 1.4 Settle the findings of two outside reviews of the proposal,
      as the maintainer decided them. Check: the maintainer's review

## 2. The spill as it is, validated

- [ ] 2.1 Have the spill read by outside reviews: the C of the nodes
      and of `runtime/spill.c`, the Rust of `tessera-spill`, of the
      words, the phases and the appending of columns, and of their
      entry points, with the specs and designs of `spill-format` and
      `hash-table`. Each is asked for faults that change a result,
      crash, leak, hang or break a promise of a spec. Write the
      findings and the fixes the maintainer accepts as tasks of this
      section; each is fixed before its code moves. Check: the
      maintainer's review of the list
- [ ] 2.2 Show by tests, then fix: an expanded value copied as a
      pointer, in the rows of a grouping that spills and in the rows
      the gather of workers sends. One copy of a value by reference, in
      C, which flattens an expanded value, serves every writer of rows,
      the count of its bytes included; the copies of the join, of its
      spill and of `runtime/rows.c` become it. Check: a test of `agg`
      with an expanded array argument and rows that spill, asserting
      the spill ran; a test of `parallel` over such a value; a C test
      that frees the expanded value before its rows are read back;
      `make installcheck`
- [ ] 2.3 Show by tests, then fix: damaged bytes that reach the C of a
      node. The sort reads a chunk of columns by its own count of
      columns; the join reads any outer block that is not one of
      values as columns; the join grows its table of value chunks by a
      number read from the file, which overflows at 2^30; a reference
      to a value is checked to its chunk, not to its last byte, and no
      node checks that a value's length ends inside its chunk. The
      node's reads take the kind and the shape they expect, and a
      chunk's number is bounded by the chunks written. Check: tests of
      `test/tessera_spill_test.c` over damaged files through the
      nodes' reads, under the sanitizers of CI; `make installcheck`
- [ ] 2.4 Refuse a partition of a shared set dropped before the set is
      finished; correct the comment that says a block of columns is
      checked before anything is written. Check: a case of
      `tessera_test_spill_error`; `make installcheck`
- [ ] 2.5 Run the tests of the C entry points of the spill under Miri,
      and fix what it finds. Check: the Miri job of CI
- [ ] 2.6 Add a counter program to `tessera-bench` for the kernels of a
      spill: packing and unpacking a chunk of records and a chunk of
      columns, appending rows by partition, the header of a block.
      Check: the program runs under `tessera-bench`; the maintainer's
      first run of counters, and a run of `runs_text` and `rows_text`
      on mains power, the bases of later comparisons

## 3. The crate

- [ ] 3.1 Make `crates/relspill` of `crates/tessera-spill`, a move and a
      rename only, with a manifest of its own, `publish = true`, and a
      build that refuses 32-bit targets. Check: `make rust-check`;
      machine code the same; `cargo package -p relhash -p relspill`
- [ ] 3.2 Open the layout of a record in `relhash` as a small public
      API, documented, with its requirement in the spec of `relhash`
      and its snapshot of the public API. Check: the diff of the
      snapshot; `make rust-check`; the maintainer's review of the API
- [ ] 3.3 Make the packing of records take the layout from `relhash`;
      the bare numbers go. Check: tests that pack and unpack records a
      table of `relhash` wrote; machine code the same
- [ ] 3.4 Move `shared_spill.rs`, `phases.rs`, their loom models and
      `spill_columns.rs` from the kernels to the crate, a move but for
      the words in shared memory, which the crate takes as a slice of
      atomic words that `tessera-capi` makes; the kernels export the
      moved names again. Check: `cargo ab moved`; machine code the
      same; `make rust-loom`; `make installcheck`
- [ ] 3.5 Replace the crate's text errors with errors of two kinds in
      one word, as `relhash` has them; a limit of the host stays the
      host's error. Check: `make rust-check`; the SQLSTATE tests of the
      suite `spill`; machine code
- [ ] 3.6 Move the writing of a set into the crate, without input or
      output: the write buffer, the lists of blocks, the trailer, and
      what a set takes after a failed write. `runtime/spill.c` keeps
      its calls of PostgreSQL and its C API. The regions come from the
      caller; the lists stay outside the measure of memory, as today.
      Check: a property test of a set over memory with writes cut by
      failures; `make installcheck`; `cargo ab measure` of the family
      `spill`
- [ ] 3.7 Move the reading of a set into the crate: a set's own lists,
      another participant's trailer and lists, a page of entries at a
      time, from the length of the file the host gives, each checked
      before the bytes it names are asked for. Check: the cases of
      damage of lists and trailers; a file written before the move
      read after it; `make installcheck`
- [ ] 3.8 Make every node pass the bounds of its blocks, a chunk by the
      plan of its level and a block of values by its largest row,
      instead of the largest allocation PostgreSQL allows; a reader of
      a shared set takes the bounds its writers agreed on. Check:
      tests of wide values and of long lists; `make installcheck`
- [ ] 3.9 Make the plan of a level one rule: its partitions, its
      chunks, its words, the expected bytes of level 0, the shift of a
      level below and the bits of a hash each level takes, with each
      node's differences as parameters. The join, the shared join, the
      grouping and the estimate of the grouping's planner use it.
      Check: `make installcheck`; machine code
- [ ] 3.10 Make one writer and one reader of a chunk of columns, and
      move the grouping's rows to them; the copy of 2.2 stays before
      the writer. Check: `make installcheck`; `cargo ab measure` of
      `rows_text` and the family `spill`
- [ ] 3.11 Move the sort's runs to the writer and the reader of 3.10.
      Check: `make installcheck`; `cargo ab measure` of `runs_text`
- [ ] 3.12 Move the join's outer rows and the gather of workers to the
      writer and the reader of 3.10. Check: `make installcheck`; the
      parallel tests of the suite `join`; `cargo ab measure` of the
      cases of `join` that spill
- [ ] 3.13 Write the machine of the join's spill, its build: the spill
      at level 0 and the partitions sent to disk; the node still
      decides the rest. Check: the machine's property test against a
      join in memory; `make installcheck`; `make crosscheck`;
      `cargo ab measure` of the family `spill`
- [ ] 3.14 Add to the join's machine the partitions on disk, joined
      whole or split to a level below. Check: the property test; the
      cases of nested levels; `make installcheck`; `make crosscheck`
- [ ] 3.15 Add the pieces and their passes, with the matched rows.
      Check: the property test; the cases of pieces and of skew;
      `make installcheck`; `cargo ab measure` of a join read in
      pieces
- [ ] 3.16 Add the tails of RIGHT and FULL joins and the output held
      across calls; the decisions of C go. Check: the property test;
      the cases of tails and early stops; `make installcheck`;
      `make crosscheck`
- [ ] 3.17 Write the machine of the grouping's spill, its groups, and
      make the node step it. Check: the property test against a
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
- [ ] 3.20 Write the machine of a build by several processes, its
      phases from `phases.rs` with what C decides of writing to disk.
      Check: the loom models, old and new; `make rust-loom`; the
      parallel tests of the suite `join`; `cargo ab measure` of a
      shared join without a spill
- [ ] 3.21 Add the agreement on a split, the publication of the filter
      before it, and the catch-up of a participant that attaches late.
      Check: loom models of each; `make rust-loom`; the parallel tests
      of the suite `join`
- [ ] 3.22 Add the rounds and the partitions taken alone, handed to the
      join's machine, and the frees. Check: loom models of the
      hand-overs; `make rust-loom`; `make crosscheck` with workers;
      `cargo ab measure` of the rounds of a parallel join
- [ ] 3.23 Make the measure of memory count the lists of blocks, the
      matched rows of a join read in pieces and the block it reads
      ahead, each by its capacity and once, with a rule of what may
      pass the limit and of what a node does when memory runs out; the
      specs that show a changed plan change with it. Check:
      `make installcheck`, every changed plan read; `cargo ab measure`
      of the family `spill`
- [ ] 3.24 Make one rule of the limit of a level below for the join and
      the grouping, if a measurement shows no loss. Check:
      `cargo ab measure` of the family `spill`; `make installcheck`
- [ ] 3.25 Make the public API what the maintainer's review of it
      decides, and keep its snapshot diffed in CI, as `relhash`'s is.
      Check: `make rust-check`; the diff of the snapshot
- [ ] 3.26 Have the crate read by outside reviews, as in 2.1, and fix
      what the maintainer accepts. Check: the maintainer's review of
      the list

## 4. The spec of the crate

- [ ] 4.1 Join the requirements of `relspill`, the moved and this
      change's, by topic; check that the specs of `hash-table` and
      `spill-format` name no call that is gone. Check:
      `openspec validate --all --strict`;
      `.github/scripts/check-specs.sh`

## 5. The documentation

- [ ] 5.1 The README as the crate's page, with an example of a spill
      over files kept in memory. Check: `cargo test -p relspill --doc`
- [ ] 5.2 Read the documentation whole, as on docs.rs, and fill what a
      caller who reads only it would miss: the actions and their
      failures, the barrier's order of memory, the bounds of a read.
      Check: `cargo doc -p relspill` with every warning an error; the
      maintainer's review

## 6. The checks of the box

- [ ] 6.1 Mark every read of bytes from a file as a trust point, list
      them in the design of `relspill`, and make the script of trust
      points check the crate, failing also on a read that goes around
      the layer. Check: the script in CI
- [ ] 6.2 Kani proofs, a family a pull request: the checks of a header
      and a trailer; the lists; the bounds of unpacking records; those
      of columns; the arithmetic of a level and of the rule of
      eviction. Each family names in the list of trust points the
      checks it covers. Check: `cargo kani -p relspill` in CI
- [ ] 6.3 The matrix of damaged files against every read of the crate
      and of the nodes. Check: its tests
- [ ] 6.4 Fuzz targets: any bytes as a file; any events and numbers to
      a machine. Check: the fuzz workflow, a minute a target on a pull
      request, and an hour a target before the close
- [ ] 6.5 Tests that no call of the crate takes memory, under a
      counting allocator. Check: `make rust-check`
- [ ] 6.6 The checks of the contract, for every rule still pending:
      the crate packed with `relhash` alone; built without its feature
      `alloc` as `no_std`, so that a file or an allocation fails to
      build; `#![forbid(unsafe_code)]` checked in CI; the script of
      trust points; the snapshot of the public API. Check:
      `.github/scripts/check-specs.sh` with no rule pending
- [ ] 6.7 Every mutant of the crate caught by its own tests, or listed
      in `.cargo/mutants.toml` with its reason. Check: a run of the
      mutants workflow for `relspill`
- [ ] 6.8 Probes of the contract's forecast, on branches never merged,
      and their counts in the design. Check: the maintainer's review

## 7. The close

- [ ] 7.1 Write the design of `relspill` for a caller of the crate.
      Check: `.github/scripts/check-specs.sh`
- [ ] 7.2 Run the fuzz targets for an hour each. Check: the run, no
      finding
- [ ] 7.3 Archive the change: the contract beside the spec, the
      roadmap, the folder deleted. Check: `openspec validate --all
      --strict`; `.github/scripts/check-specs.sh`
- [ ] 7.4 The pull request that closes the change gives the outcome.
      Check: the maintainer's approval
