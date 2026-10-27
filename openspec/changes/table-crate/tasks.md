## 0. How the work goes

Every task below is a pull request of one to three commits, read by
the maintainer: up to 3.17 into the branch `relhash`, rebased onto
`main` as it moved and brought into it fast-forward after 3.17; from
3.18 on into `main`. A task that moves a file named by the spec or the
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
- [x] 1.4 Write the crate's contract, `contract.md` in this change's
      folder: the rules of "The contract of the crate", each with its
      check or `pending` until its task, and the forecast. Check:
      `.github/scripts/check-specs.sh`
- [x] 1.5 Make `check-specs.sh` read every contract, in
      `openspec/specs/` and in open changes: each "Checked by" names a
      file that holds its text, as a "Verified by" does, `pending` only
      in an open change; in a pull request it reads the contracts of
      the base, and a rule of the base that the head drops or whose
      check it drops fails. Check: the script over a contract with a
      wrong path, a dropped rule and a dropped check

## 2. The table's ties cut inside the kernels

- [x] 2.1 Give the table its own copy of the prefetch hint. Check:
      machine code the same as the base's
- [x] 2.2 Add the cursor that gives a payload by reference, and make the
      counts and the folds of int2, int4 and int8 states go through it.
      Check: the aggregate tests of
      `crates/tessera-kernels/tests/table.rs`; machine code
- [x] 2.3 Add to that cursor the check of a reference now and its
      payload later, and make the sums and extremes of numeric states go
      through it. Check: the aggregate tests; machine code
- [x] 2.4 Add the merge of a chunk's records by a caller's rule, and
      make the merge of groups read back go through it, with its kinds
      of state as the rule. Check: the merge tests; machine code
- [x] 2.5 Add the cursor that reads a record by reference, and make the
      items of a sort come from the scan and that cursor. Check:
      `crates/tessera-kernels/tests/sort.rs`; machine code
- [x] 2.6 Give the phases and the words of a spill orderings of their
      own, and give the loom models of the phases and the rounds the
      model table under `--cfg loom`. Check: `make rust-loom`
- [x] 2.7 Move the folds, the kinds of a merge and `order.rs` out of
      `table/`, a move only. Check: `cargo ab moved`; machine code the
      same; `make installcheck`
- [x] 2.8 Move the keys read from columns, `phases.rs`,
      `shared_spill.rs` and their loom models out of `table/`, a move
      only. Check: `cargo ab moved`; machine code the same;
      `make rust-loom`; `make installcheck`
- [x] 2.9 Make the generic aggregates of the grouping node take the
      payloads of their groups from `tess_table_payloads`, one call for
      the rows of a batch that need them, and drop the node's own
      reading of a record by an offset, which the contract's rule
      "References checked by the crate" forbids. Check:
      `make installcheck`; `cargo ab measure` of the grouping cases of
      the family `anyagg` against the base, no case slower
- [x] 2.10 Add `tess_table_chunk_used`, which gives a chunk's used mark
      checked, as the requirement "The used mark of a chunk" of
      `hash-table` says, with its tests named in the requirement; make
      the spills of the grouping and of the join read a chunk's mark
      through it instead of by themselves, which the contract's rule
      "References checked by the crate" forbids. Check: the tests of
      the C API in `crates/tessera-capi/tests/table.rs`;
      `make installcheck`

## 3. The crate

- [x] 3.1 Make `crates/relhash` of `row_mask.rs`, `bits.rs` and
      `bitmap.rs` of `tessera-core`, a move only; `tessera-core`
      exports the moved names again; add the check that no crate but
      `relhash` defines a mask of rows, and name it in the contract's
      rule "The masks of rows are the crate's". Check:
      `make rust-check`; machine code the same on every program and
      module
- [x] 3.2 Give the table's unit tests, its loom model and the example of
      its module a source of keys and a hash of their own, and move the
      tests of the kinds of a merge to `accumulate.rs`, so that nothing
      in `table/` needs the kernels. Check: `cargo test -p
      tessera-kernels --lib`; `cargo test -p tessera-kernels --doc`;
      `make rust-loom`
- [x] 3.3 Move `table/` into the crate as it is, the format and the calls
      at once: `header.rs`, `region.rs`, `record.rs`, `lanes.rs`,
      `batch.rs`, `keys.rs`'s source of keys, what is left of
      `exclusive.rs`, `bloom.rs`, `marks.rs`, `mod.rs`, `local.rs`
      behind a feature `alloc` on by default, the loom models of the
      table, the filter and the marks, and `model.rs` with loom a
      dependency under `--cfg loom`, a move only; `tessera-kernels`
      exports the moved names again. The format alone would have made
      the crate's inner parts public for the calls still in the kernels.
      After the maintainer's review of the move, in the same pull
      request: a file for each part (the chunks, the batch, the probe of
      a word, the walk over a chunk, the tests, the loom models in
      `table/loom/`); the names `OwnedTable`, `writer.rs`,
      `Header::with_capacity` and a `Place` of a chunk and a byte; one
      rule for the size of a record, which makes `tess_table_chunk_used`
      refuse sizes no table's records have; each check and step written
      once; no kinds of keys in the table, its format or its C calls,
      the format version 2; the texts of the modules for a developer,
      with pictures. Check: `make rust-check`; machine code the same but
      where a commit names its change; `make rust-loom`;
      `make installcheck`
- [x] 3.4 Make the public API what the maintainer's review of it
      decided: the table's items at the crate's root; the calls over
      chunks before a table has an index as one type, `ChunkWriter`;
      references named references in the parameters and the texts;
      the format's magic `RELHASH\0`; `BITS_PER_RECORD` closed. Check:
      `make rust-check`; machine code the same but where a commit
      names its change; `cargo public-api -p relhash`
- [x] 3.5 Make a lookup one value, `Matches`: the rows still in it, a
      mask the calls narrow in place, and each row's reference. The
      probe, the next match, the next in a group and the Bloom filter's
      probes narrow its rows and return how many are left; the gathers
      read its rows and references. The C entry points copy their mask
      of rows into their mask of results and call the same. Check:
      `make rust-check`; `make installcheck`; machine code, every loop
      that changes with its reason; the maintainer's run of counters on
      `table_int32` and `table_large`; `cargo ab measure` of `join`
- [x] 3.6 Let the C entry points of the probe, the next match, the
      next in a group and the Bloom filter's probes narrow a mask in
      place: `found` may be `rows` itself, and then nothing is copied.
      The nodes use it where they copy a mask before such a call: the
      steps over a key's records, the filter before the probe. The
      hash-table spec says so of each call. Check: `make installcheck`;
      `cargo ab measure` of `join`, `dup` among its cases
- [x] 3.7 Replace the crate's text errors by errors of two kinds of at
      most 8 bytes, with a compile-time assertion of the size; the call
      site of a reference's check chooses the kind; the check of a
      header that its fast and full forms judge apart becomes a debug
      assertion; `tessera-capi` writes the same messages from them; the
      tests compare kinds and rules. Check: `make rust-check`;
      `make installcheck`; machine code; the maintainer's run of
      counters on `table_int32`, `table_large` and the aggregate
      programs
- [x] 3.8 Move the table's tests into the crate, over a source of keys
      of the crate's own, and take the hooks for tests out of the
      public API: `OwnedTable::index_words`, `chunk_words`,
      `add_chunk_copy`, `insert_grouped` and `chunk_set`. Check:
      `cargo test -p relhash`; `cargo public-api -p relhash`
- [x] 3.9 Make a batch to place one value, as a lookup is: `Batch`
      holds its mask of pending rows by value, has one lifetime, and is
      made from the caller's words or from a mask; its references are
      `refs` in the whole crate; `OwnedTable::insert` and
      `OwnedTable::find_or_insert` take the rows to place as a mask
      they only read. Check: `make rust-check`; machine code, every loop
      that changes with its reason; `cargo public-api -p relhash`
- [x] 3.10 Make `find_or_insert` and its form by partition, in the crate
      and in the C API, add the rows whose record they made to the mask
      of inserted rows, which the caller clears once a batch; the
      grouping nodes and the owning table stop adding the masks up.
      Check: `make rust-check`; `make installcheck`; machine code;
      `cargo ab measure` of the grouping cases of `win`, no case
      slower
- [x] 3.11 Keep a table whole when a call that places rows fails part
      way: `append`, `append_columns`, `find_or_insert` and their forms
      by partition store the used mark of every chunk they wrote on
      every exit; the rows placed before the error leave the pending
      mask with their references and, when they made a record, enter
      the mask of inserted rows; `link_counting` counts what it linked
      and moves `*from` past it. Check: for each of these calls, a
      source of keys that fails at a chosen word or a partition's chunk
      refused in the middle of a word, then the same batch placed again;
      `make rust-check`; machine code
- [x] 3.12 Keep the owning table whole under any order of its calls:
      an insertion links what it appended and nothing else, and
      `OwnedTable::new` refuses chunks too small for a record. Check: a
      property test of random sequences of `insert`, `find_or_insert`,
      `regrow` and `reset` against a model of the records;
      `cargo test -p relhash`
- [x] 3.13 Take the writes of chunks and the index off the shared
      handle: `append`, `append_columns`, `link` and `link_counting`
      safe on `TableMut` and `unsafe` on `Table`, whose contract names
      the chunks the caller writes and that no record of them is
      borrowed; `scan` on `TableMut`; the documentation of `record`, the
      gathers and the next match says what a reference past a chunk's
      used mark reads while a writer writes it. Check: a compile-fail
      example of a record borrowed across an append; Miri over a reset
      and a reuse of an old reference; `cargo public-api -p relhash`;
      `make installcheck`
- [x] 3.14 Check, in a build with debug assertions, that a source gives
      0 in the slot of every key it marks NULL, and say in `KeySource`
      why, and what a source that cannot give a word leaves behind.
      Check: a source that leaves values in NULL slots; machine code
- [x] 3.15 Make the smaller changes of the API: `Marks::set` and
      `Marks::word` safe; `check_rows` out of the crate's API;
      `init_chunk` and `chunk_used` with the other calls over chunks;
      `CombineStop` named for `merge`; `Batch::refs`; `mark` and
      `scan_unmarked` taking the record size from the table; the
      documentation of `Batch::new` binding its source of keys. Check:
      `cargo public-api -p relhash`; `make rust-check`;
      `make installcheck`
- [x] 3.16 Stop a walk down a chain at a cycle by Brent's method, and
      write into the design what the counters of the probe and the find
      show it costs, once the maintainer accepts it. Check: cycles
      of two and of many records under a damaged count; machine code;
      the maintainer's run of counters on `table_int32` and
      `table_large`
- [x] 3.17 Strengthen the tests: every refusal compared by its kind and
      rule, not by `is_err()`; the longest chunk filled to its last
      record, chunk 32767, and the largest record end to end; a loom
      model of `link_counting` beside a third participant's link; Miri
      over the doc tests in CI; the build without `alloc` free of
      warnings. Check: `cargo test -p relhash`; `make rust-loom`; CI
- [x] 3.18 Write the contracts of memory: `ChunkWriter::new` `unsafe`,
      its writer the one writer of the chunks it writes, nothing
      borrowing their records or payloads while one of its calls runs;
      `Chunks::new`, `Table::attach` and `TableMut::create` asking that
      the index, the chunks and the arrays that name them do not
      overlap, that a chunk has one writer across every table and chunk
      writer over it, and that a table that has itself to itself has
      its chunks too; the C contracts and the SAFETY of the callers in
      `tessera-capi` and `tessera-kernels` to match. Check: a
      compile-fail example of a chunk writer made in safe code; `cargo
      public-api -p relhash`; `make installcheck`
- [x] 3.19 Link what an insertion of the owning table appended before
      it returns an error, and say on `insert` and `find_or_insert`
      what an error leaves behind. Check: the model of the owning
      table's calls with sources that fail at a chosen word, in the
      first chunk and after growing into a second, and at the limit of
      chunks; `cargo test -p relhash`
- [x] 3.20 Give C pointers to payloads within Rust's rules of
      aliasing: a call of the crate that checks each selected row's
      reference and writes a raw pointer to its record's payload, made
      from the chunk's base, two rows of one record the same pointer;
      `tess_table_payloads` through it. Check: Miri over two rows of
      one record written through their pointers after the call, by the
      crate's call and by the C entry point; `make installcheck`;
      machine code
- [x] 3.21 Bound a walk over matches: `Matches` counts the steps to a
      next match or a next in a group, and a step past the count of
      records fails as a damaged chain; `tess_table_next_match` and
      `tess_table_next_in_group` take the count from the caller, and
      Tessera's joins give it. Check: a record whose next names itself
      and a cycle of two equal keys, stepped through in Rust and in C;
      `make installcheck`; machine code, since no benchmark steps to a
      next match
- [x] 3.22 Write the rules of the calls over every chunk: the contracts
      of `Table::attach` and `Chunks::new` name `bloom`,
      `try_build_bloom` and both forms of `scan_unmarked`, and the
      barrier after every writer's appends; `regrow`, `bloom`,
      `clear_key`, a walk and `merge` say that their chunks are the
      table's whole set of records. Check: `RUSTDOCFLAGS="-D warnings"
      cargo doc -p relhash --no-deps`; the maintainer's review of the
      texts
- [x] 3.23 Check every chunk before the first change in `regrow`,
      `clear_key`, `bloom` and `try_build_bloom`, and say in the
      documentation that a table a call found damaged is dropped or
      reset, unlike a table after an error of the caller's rule.
      Check: a larger index and a cleared key given a wrong last chunk
      leave the table as it was; machine code
- [x] 3.24 Mend the smaller faults: a walk from a cursor past its
      chunk's used mark, or into an empty output, fails as a wrong
      call; a count of rows by partition that would overflow fails
      before its row; the C gather of many words checks its whole range
      first; the most records of a find or insert and of a merge
      written on those calls and on `regrow`, and the owning table
      failing at that limit, tested at the decision without filling
      it; a shared filter whose build fails documented as never ready.
      Check: each case, in debug and release builds; `cargo test -p
      relhash`; `cargo test -p tessera-capi`; machine code
- [x] 3.25 Settle the interface before it is frozen: `OwnedTable`
      `Send`; `Stats`, `Split`, `Record` and `MergeStop`
      `#[non_exhaustive]`; the documentation of a caller's reference
      that reads as damage, of a source's own error behind the error of
      a key not read, of `Cursor::from_raw`, `Marks::set`, of `Matches`
      after a key not read in the middle of a probe and of
      `Matches::rows_mut`, of a merge whose rule fails after it changed
      a payload, of the six public fields without it, and of a mask's
      error, which allocates nothing. Check: an owning table filled on
      one thread and probed on another; `RUSTDOCFLAGS="-D
      missing_docs"`; `cargo public-api -p relhash`; `make installcheck`
- [x] 3.26 Add the tests the reviews found missing: a healthy chain of
      300 records of one hash walked to its end by row, by word, down
      its matches and by a find or insert; the crate's tests that need
      no owning table, its documentation and its doc tests built and
      run without `alloc`, in CI, with an example over the caller's
      blocks. Check: `cargo test -p relhash`; `cargo test -p relhash
      --no-default-features`; `cargo doc -p relhash
      --no-default-features`; the mutants of `find_long` and
      `finish_alone`
- [x] 3.27 Make the crate one of its own, without publishing it on
      crates.io: `publish = true` in its manifest, which marks its API
      as a surface of a spec; a target whose pointers are not 64 bits
      wide refused with `compile_error!`; in CI a build for a 32-bit
      target that must fail, the crate's tests alone, a build without
      `alloc`, a dry run of `cargo publish`, and a snapshot of the
      crate's public API that CI compares; these checks named in the
      contract's rules "Free of Tessera" and "A small surface". Check:
      `cargo package -p relhash`; CI

## 4. The spec of the crate

- [x] 4.1 Open, have approved and close the change `relhash-spec-table`:
      the index, chunks and references, a record, and the calls of
      `hash-table` move to the capability `relhash`, nine
      requirements, in the crate's terms and with the same tests; the
      four this change rewrites move as it has them, and their entries
      and its purpose of `relhash` leave its deltas; `hash-table` gets
      its requirement of the C API over the crate. Check: the tasks of
      that change
- [x] 4.2 Open, have approved and close the change
      `relhash-spec-partitions`: the partitions, the Bloom filter, the
      marks and what makes a table damaged move the same way, ten
      requirements, the last as "A damaged table"; the three of
      partitions and the errors that this change rewrites move as it
      has them, and their entries leave its delta; `hash-table` keeps
      the status of an error, a participant that stops while it
      probes, and the C calls over partitions, filters and marks in
      its requirement of the C API; the designs split. Check: the
      tasks of that change
- [x] 4.3 Set this change's spec on top of the moved requirements: an
      entry modifies "A damaged table" of `relhash` to list what makes
      a table damaged, and "Errors of two kinds" points to it instead
      of listing it again. Join the requirements of both specs by
      topic, one to five scenarios each, as `openspec/config.yaml` now
      asks: a requirement of `main` that takes in another is modified
      and keeps its place, the one it takes in is removed; the clauses
      about C still in scenarios of `relhash` move to "The C API over
      the crate". Check: `openspec validate table-crate --strict`;
      `.github/scripts/check-specs.sh`

## 5. The documentation

- [x] 5.1 Write the crate's README as the page of its documentation:
      what the table is for, an example without Tessera over the owning
      table, the caller's promises in a line with a link, the advice to
      build with LTO and to use a hash with a secret seed for keys from
      outside, and links to the spec and the design. Check:
      `cargo test --doc -p relhash`
- [x] 5.2 Make the documentation complete by itself, as a map of the
      API: each public item with what it is for, takes and gives, and
      the rules, layouts and limits that the spec of `relhash` holds
      now, moved into it, with examples where they help; the README
      with the caller's whole promise. Cut each requirement of `relhash`
      to its promise and links to the items that hold the rest, and
      check those links in `check-specs.sh`. Cut the module texts that
      restate the documentation or the design down to links; set
      `#![deny(missing_docs)]`. Check:
      `RUSTDOCFLAGS="-D warnings" cargo doc -p relhash --no-deps`;
      `.github/scripts/check-specs.sh`
- [x] 5.3 Add the build of the documentation and its examples to CI.
      Check: CI

## 6. The checks of the box

- [x] 6.1 Mark every read of the index or a chunk as a trust point, add
      the script that compares the marks with their list, and write the
      list into the design of `relhash`; forbid unsafe code in every
      other module; name these checks in the contract's rules "One way
      to memory" and "Unsafe code where it is proven". Check: the
      script; CI
- [x] 6.2 Write the Kani proofs of the checks and their arithmetic, add
      the Kani job to CI, and name the proofs in "Any bytes, a bounded
      call" and, in the design of `relhash`, at each trust point and
      each module of unsafe code they cover. Check:
      `cargo kani -p relhash`
- [x] 6.3 Write the matrix of damage with guard words around the blocks
      and calls after the first error, run a few cases under Miri, and
      name it in "Any bytes, a bounded call" and "Errors of two kinds",
      and its rows at the trust points of the design of `relhash`;
      fix what it finds, each in a commit with its case. Check:
      `cargo test -p relhash`; `cargo miri test -p relhash`
- [x] 6.4 Add the fuzz targets, a minute each on every pull request and
      a long run on request, and name them in "Any bytes, a bounded
      call"; report the downloads, reverse dependencies, last release
      and stars of `libfuzzer-sys`. Check: `cargo fuzz run <target> --
      -max_total_time=60` for each target
- [x] 6.5 Add the tests of an allocator that counts (the check of the
      contract's rule "No memory taken"), of one sequence run twice, of
      a handle sent to another thread and of two payloads open at once
      (both must not compile), of the masks, of the cursors and of the
      merge, and name them in their requirements. Check:
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
- [ ] 7.3 Archive the change, move its `contract.md` and its
      `public-api.txt` to `openspec/specs/relhash/`, the latter's path
      in CI with it, delete the folder the archive makes, and put what
      is left into `openspec/roadmap.md`. In the same commit put each
      added requirement beside those of its topic, and point the links
      of both designs to the requirements as they then stand. Check:
      `openspec validate --all --strict`;
      `.github/scripts/check-specs.sh`
- [ ] 7.4 Open the pull request that closes the change, into `main`.
      Check: CI; the maintainer's review of the outcome
