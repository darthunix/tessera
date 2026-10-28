## 0. How the work goes

Every task below is a pull request into `main` of one to three commits,
read by the maintainer. A task that moves a file named by a spec or a
design edits those paths in the same commit. Each commit that moves or
changes Rust is checked by `make rust-check` and by `tessera-bench
--base <previous> --disasm` on every program and with `--module`; no
loop may grow without a reason written down. Each task that changes a
node runs `make installcheck` and `make crosscheck`, the latter with
the spills its generator makes.

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
      its writer out of `small-c-leftovers`. Check:
      `.github/scripts/check-specs.sh`

## 2. The spill as it is, validated

- [ ] 2.1 Have the spill read by outside reviews: the C of the nodes
      and of `runtime/spill.c`, the Rust of `tessera-spill`, of the
      words, the phases and the appending of columns, and of their
      entry points, with the specs and designs of `spill-format` and
      `hash-table`. Each is asked for faults that change a result,
      crash, leak, hang or break a promise of a spec. Write the
      findings and the fixes the maintainer accepts as tasks of this
      section. Check: the maintainer's review of the list
- [ ] 2.2 Show by a test, then fix: the join's two thresholds, so that
      reading a partition in pieces follows
      `tessera.join_spill_split_room` as the split does. Check: a test
      of the suite `join` with the setting changed; `make installcheck`
- [ ] 2.3 Show by a test, then fix: an expanded value in the rows of a
      grouping that spills, copied flat as the join copies it. Check:
      a test of the suite `agg`; `make installcheck`
- [ ] 2.4 Show by a test, then fix or state as the caller's duty: a
      shared set dropped before it is finished; a block of columns that
      flushes the buffer before its header is checked. Check: the
      tests of `test/tessera_spill_test.c`; `make installcheck`
- [ ] 2.5 Run the tests of the C entry points of the spill under Miri,
      and fix what it finds. Check: the Miri job of CI
- [ ] 2.6 Add a counter program to `tessera-bench` for the kernels of a
      spill: packing and unpacking a chunk of records and a chunk of
      columns, appending rows by partition, the header of a block.
      Check: the program runs under `tessera-bench`; the maintainer's
      first run of counters, the base of later comparisons

## 3. The crate

- [ ] 3.1 Make `crates/relspill` of `crates/tessera-spill`, a move and a
      rename only; its packing of records takes the layout of a record
      from `relhash`. Check: `make rust-check`; machine code the same
- [ ] 3.2 Move `shared_spill.rs`, `phases.rs`, their loom models and
      `spill_columns.rs` from the kernels to the crate, a move only;
      the kernels export the moved names again. Check: `cargo ab
      moved`; machine code the same; `make rust-loom`;
      `make installcheck`
- [ ] 3.3 Replace the crate's text errors with errors of two kinds in
      one word, as `relhash` has them. Check: `make rust-check`; the
      SQLSTATE tests of the suite `spill`; machine code
- [ ] 3.4 Move the file of a set into the crate, without input or
      output: the write buffer, the lists of blocks, the trailer, the
      checks of a read; `runtime/spill.c` keeps its calls of
      PostgreSQL and its C API. The regions come from the caller; the
      lists stay outside the measure of memory, as today. Check:
      `make installcheck`; a file written before the move read after
      it; `cargo ab measure` of the family `spill`
- [ ] 3.5 Make the plan of a level one rule: its partitions, its
      chunks, its words, the expected bytes of level 0 and the shift of
      a level below, with each node's differences as parameters. The
      join, the shared join and the grouping make their levels through
      it. Check: `make installcheck`; machine code
- [ ] 3.6 Make one writer and one reader of a chunk of columns, for the
      grouping's rows, the sort's runs, the join's outer rows and the
      gather of workers. Check: `make installcheck`; `cargo ab measure`
      of `runs_text`, `rows_text` and the family `spill`
- [ ] 3.7 Write the machine of the join's spill and make the node step
      it; the decisions of C go. Check: the machine's property test
      against a join in memory; `make installcheck`;
      `make crosscheck`; `cargo ab measure` of the family `spill` and
      of the cases of `join` that spill
- [ ] 3.8 Write the machine of the grouping's spill, its groups and its
      rows, and make the node step it. Check: the property test
      against a grouping in memory; `make installcheck`;
      `cargo ab measure` of the family `spill`
- [ ] 3.9 Write the machine of the sort's runs and make the node step
      it. Check: the property test against a sort in memory;
      `make installcheck`; `cargo ab measure` of `runs_text` and of
      the sorts of the family `sort`
- [ ] 3.10 Write the machine of a build by several processes and its
      rounds, from `phases.rs` and what C decides of them, and make the
      shared join step it. Check: the loom models, old and new;
      `make rust-loom`; the parallel tests of the suite `join`;
      `make crosscheck` with workers
- [ ] 3.11 Make the measure of memory count the lists of blocks, the
      matched rows of a join read in pieces and the block it reads
      ahead, with a reserve for them in the rule. Check: `make
      installcheck`, every changed plan read; `cargo ab measure` of the
      family `spill`
- [ ] 3.12 Make one rule of the limit of a level below for the join and
      the grouping, if a measurement shows no loss. Check:
      `cargo ab measure` of the family `spill`; `make installcheck`
- [ ] 3.13 Make the public API what the maintainer's review of it
      decides. Check: `make rust-check`; `cargo public-api -p relspill`
- [ ] 3.14 Have the crate read by outside reviews, as in 2.1, and fix
      what the maintainer accepts. Check: the maintainer's review of
      the list

## 4. The spec of the crate

- [ ] 4.1 A change of its own moves the requirements of the format from
      `spill-format` to `relspill`, as the code has them. Check:
      `openspec validate --all --strict`;
      `.github/scripts/check-specs.sh`
- [ ] 4.2 A change of its own moves the requirements of the words of a
      spill, the levels, the phases and the rounds from `hash-table` to
      `relspill`. Check: the same
- [ ] 4.3 Set this change's requirements on top of the moved ones, and
      join them by topic. Check: the same

## 5. The documentation

- [ ] 5.1 The README as the crate's page, with an example of a spill
      over files kept in memory. Check: `cargo test -p relspill --doc`
- [ ] 5.2 Document every public item, complete by itself;
      `#![deny(missing_docs)]`. Check: `cargo doc -p relspill` with
      every warning an error
- [ ] 5.3 Build the documentation and run its examples in CI. Check:
      CI

## 6. The checks of the box

- [ ] 6.1 Mark every read of bytes from a file as a trust point, list
      them in the design of `relspill`, and make the script of trust
      points check the crate. Check: the script in CI
- [ ] 6.2 Kani proofs of the checks of a header, a trailer and the
      lists, of the bounds of unpacking, and of the arithmetic of a
      level and of the rule of eviction. Check: `cargo kani -p
      relspill` in CI
- [ ] 6.3 The matrix of damaged files against every read. Check: its
      test
- [ ] 6.4 Fuzz targets: any bytes as a file; any events and numbers to
      a machine. Check: the fuzz workflow, a minute a target on a pull
      request, and an hour a target before the close
- [ ] 6.5 The property tests of the machines against a model, with
      memory counted and steps cut by errors; tests that no call takes
      memory. Check: `make rust-check`
- [ ] 6.6 Loom models of the machine of several processes with its
      files, its catch-up, its hand-over to the rounds, its stop and its
      frees. Check: `make rust-loom`
- [ ] 6.7 Miri over every test of the crate. Check: the Miri job of CI
- [ ] 6.8 Every mutant of the crate caught by its own tests, or listed
      in `.cargo/mutants.toml` with its reason. Check: a run of the
      mutants workflow for `relspill`
- [ ] 6.9 Probes of the contract's forecast, on branches never merged,
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
