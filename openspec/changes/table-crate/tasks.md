## 0. How the work goes

Every task below is a pull request into the branch `relhash`, of one to
three commits, read by the maintainer; `main` is merged into the branch
as it moves, and one pull request brings the branch into `main` at the
end (task 7.4). A task that moves a file named by the spec or the
design of `hash-table` edits those paths in the same commit. Each
commit of sections 2 and 3 is checked by `make rust-check` and by
`tessera-bench --base <previous> --disasm` on every program and with
`--module`; no loop may grow without a reason written down.

## 1. The proposal

- [x] 1.1 Write the proposal, the spec, the design and the tasks of
      this change. Check: `openspec validate table-crate --strict`;
      `.github/scripts/check-specs.sh`
- [x] 1.2 Add to `CONTRIBUTING.md` and `openspec/config.yaml` the fifth
      surface of a spec, the public API of a crate of the workspace
      meant for use outside Tessera, one with `publish = true`; that
      such an API changing needs a change; and that a crate's
      documentation links to its spec for a rule and does not repeat
      it. Check: `.github/scripts/check-specs.sh`
- [x] 1.3 Make the branch `relhash` from `main`, and open this proposal
      as the first pull request into it. Check: CI on the pull request
- [ ] 1.4 Write the crate's contract, `contract.md` in this change's
      folder: the rules of "The contract of the crate", each with its
      check or `pending` until its task, and the forecast. Check:
      `.github/scripts/check-specs.sh`
- [ ] 1.5 Make `check-specs.sh` read every contract, in
      `openspec/specs/` and in open changes: each "Checked by" names a
      file that holds its text, as a "Verified by" does, `pending` only
      in an open change; in a pull request it reads the contracts of
      the base, and a rule of the base that the head drops or whose
      check it drops fails. Check: the script over a contract with a
      wrong path, a dropped rule and a dropped check

## 2. The table's ties cut inside the kernels

- [ ] 2.1 Give the table its own copy of the prefetch hint. Check:
      machine code the same as the base's
- [ ] 2.2 Add the cursor that gives a payload by reference, and make the
      counts and the folds of int2, int4 and int8 states go through it.
      Check: the aggregate tests of
      `crates/tessera-kernels/tests/table.rs`; machine code
- [ ] 2.3 Add to that cursor the check of a reference now and its
      payload later, and make the sums and extremes of numeric states go
      through it. Check: the aggregate tests; machine code
- [ ] 2.4 Add the merge of a chunk's records by a caller's rule, and
      make the merge of groups read back go through it, with its kinds
      of state as the rule. Check: the merge tests; machine code
- [ ] 2.5 Add the cursor that reads a record by reference, and make the
      items of a sort come from the scan and that cursor. Check:
      `crates/tessera-kernels/tests/sort.rs`; machine code
- [ ] 2.6 Give the phases and the words of a spill orderings of their
      own, and give the loom models of the phases and the rounds the
      model table under `--cfg loom`. Check: `make rust-loom`
- [ ] 2.7 Move the folds, the kinds of a merge and `order.rs` out of
      `table/`, a move only. Check: `cargo ab moved`; machine code the
      same; `make installcheck`
- [ ] 2.8 Move the keys read from columns, `phases.rs`,
      `shared_spill.rs` and their loom models out of `table/`, a move
      only. Check: `cargo ab moved`; machine code the same;
      `make rust-loom`; `make installcheck`

## 3. The crate

- [ ] 3.1 Make `crates/relhash` of `row_mask.rs`, `bits.rs` and
      `bitmap.rs` of `tessera-core`, a move only; `tessera-core`
      exports the moved names again. Check: `make rust-check`; machine
      code the same on every program and module
- [ ] 3.2 Move the format of the table into the crate: `header.rs`,
      `region.rs`, `record.rs`, `lanes.rs` and what `mod.rs` holds of
      them, a move only; `tessera-kernels` exports the moved names
      again. Check: `make rust-check`; machine code the same;
      `make rust-loom`; `make installcheck`
- [ ] 3.3 Move the calls into the crate: `batch.rs`, `keys.rs`'s source
      of keys, what is left of `exclusive.rs`, `bloom.rs`, `marks.rs`,
      `local.rs` behind a feature `alloc` on by default, and the loom
      models of the table, the filter and the marks, a move only.
      Check: as 3.2
- [ ] 3.4 Set `publish = true` for the crate and refuse a target whose
      pointers are not 64 bits wide with `compile_error!`; add to CI a
      build for a 32-bit target that must fail, the crate's tests alone,
      a build without `alloc`, and a dry run of `cargo publish`. Check:
      `cargo package -p relhash`; CI
- [ ] 3.5 Replace the crate's text errors by errors of two kinds of at
      most 8 bytes, with a compile-time assertion of the size; the call
      site of a reference's check chooses the kind; the check of a
      header that its fast and full forms judge apart becomes a debug
      assertion; `tessera-capi` writes the same messages from them; the
      tests compare kinds and rules. Check: `make rust-check`;
      `make installcheck`; machine code; the maintainer's run of
      counters on `table_int32`, `table_large` and the aggregate
      programs
- [ ] 3.6 Move the table's tests into the crate, over a source of keys
      of the crate's own. Check: `cargo test -p relhash`

## 4. The spec of the crate

- [ ] 4.1 Open, have approved and close the change `relhash-spec-table`:
      the index, chunks and references, a record, and the calls of
      `hash-table` move to the capability `relhash`, nine
      requirements, in the crate's terms and with the same tests;
      `hash-table` gets its requirement of the C API over the crate.
      Check: the tasks of that change
- [ ] 4.2 Open, have approved and close the change
      `relhash-spec-partitions`: the partitions, the Bloom filter, the
      marks and what makes a table damaged move the same way, nine
      requirements; the designs split. Check: the tasks of that change
- [ ] 4.3 Set this change's spec on top of the moved requirements: an
      entry removes the part of "Errors of a call" that moved, which
      "Errors of two kinds" replaces. Check:
      `openspec validate table-crate --strict`;
      `.github/scripts/check-specs.sh`

## 5. The documentation

- [ ] 5.1 Write the crate's README as the page of its documentation:
      what the table is for, an example without Tessera over the owning
      table, the caller's promises in a line with a link, the advice to
      build with LTO and to use a hash with a secret seed for keys from
      outside, and links to the spec and the design. Check:
      `cargo test --doc -p relhash`
- [ ] 5.2 Document every public item as a map of the API, with links to
      the spec instead of its rules, and examples where they help; cut
      the module texts that restate the spec or the design down to
      links; set `#![deny(missing_docs)]`. Check:
      `RUSTDOCFLAGS="-D warnings" cargo doc -p relhash --no-deps`
- [ ] 5.3 Add the build of the documentation and its examples to CI.
      Check: CI

## 6. The checks of the box

- [ ] 6.1 Mark every read of the index or a chunk as a trust point, add
      the script that compares the marks with their list, and write the
      list into the design of `relhash`. Check: the script; CI
- [ ] 6.2 Write the Kani proofs of the checks and their arithmetic, add
      the Kani job to CI, and name the proofs in "Any bytes, a bounded
      call". Check: `cargo kani -p relhash`
- [ ] 6.3 Write the matrix of damage with guard words around the blocks
      and calls after the first error, run a few cases under Miri, and
      name it in "Any bytes, a bounded call" and "Errors of two kinds";
      fix what it finds, each in a commit with its case. Check:
      `cargo test -p relhash`; `cargo miri test -p relhash`
- [ ] 6.4 Add the fuzz targets, a minute each on every pull request and
      a long run on request, and name them in "Any bytes, a bounded
      call"; report the downloads, reverse dependencies, last release
      and stars of `libfuzzer-sys`. Check: `cargo fuzz run <target> --
      -max_total_time=60` for each target
- [ ] 6.5 Add the tests of an allocator that counts, of one sequence run
      twice, of a handle sent to another thread and of two payloads open
      at once (both must not compile), of the masks, of the cursors and
      of the merge, and name them in their requirements. Check:
      `cargo test -p relhash`; `.github/scripts/check-specs.sh`
- [ ] 6.6 Run Miri over all the tests of the crate in CI. Check: CI
- [ ] 6.7 Run every mutant of the crate in CI on request; give each
      survivor a test, and list the equivalent ones with their reasons
      in `.cargo/mutants.toml`. Check: the mutants workflow on request,
      no survivor left
- [ ] 6.8 Run a probe of each change of the contract's forecast on a
      branch that is never merged, count the parts and interfaces it
      touched, and write the counts into the design; a probe that
      touches more than forecast is a deviation, handled as
      `CONTRIBUTING.md` says. Check: the probes' branches and counts

## 7. The close

- [ ] 7.1 Bring the design of `relhash` up to date for a caller of the
      crate, with nothing of how the table moved: the crate's boundary,
      its errors, its calls for a caller, its owning table, its checks,
      Files and Tests. Check:
      `.github/scripts/check-specs.sh`
- [ ] 7.2 The long fuzz run of one to two hours, on request, with no
      finding. Check: the run's log
- [ ] 7.3 Archive the change, move its `contract.md` to
      `openspec/specs/relhash/`, delete the folder the archive makes,
      and put what is left into `openspec/roadmap.md`. Check:
      `openspec validate --all --strict`;
      `.github/scripts/check-specs.sh`
- [ ] 7.4 Merge `main` into the branch a last time and open the pull
      request of the branch into `main`. Check: CI; the maintainer's
      review of the outcome
