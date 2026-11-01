## 0. How the work goes

Every task is one commit on the branch `float-kernels-in-rust`, in the
worktree of that name. The whole change, from the proposal to its
close, is one pull request; code starts after the proposal is approved.
Each commit passes `make rust-check` and
`.github/scripts/check-specs.sh`, run with the version of OpenSpec that
`ci.yml` names; a commit that touches C also passes `make && make
install` and `make installcheck
EXTRA_REGRESS_OPTS="--temp-instance=$PWD/test/tmp_check"`. The check
named in a task comes on top.

## 1. The proposal

- [x] 1.1 Write the proposal, the design and the tasks of this change,
      with `skip_specs: true`. Check:
      `openspec validate float-kernels-in-rust --strict`;
      `.github/scripts/check-specs.sh`

## 2. What the C version answers

- [x] 2.1 Add the three float cases of the design's measurement plan to
      the family `win` of `bench/pg`, with their lines in
      `bench/pg/README.md`. Check: `bench/pg/run.sh measure win` with
      `CASES='^float_'` and `REPETITIONS=1` runs them, and `plans.txt`
      shows the float functions computed by Tessera's node.
- [x] 2.2 Add to the float section of the suite `types` the rows the
      design's "Tests" lists (a subnormal, `NaN / 0`, `0 / 0`,
      infinity times zero, a float4 product that overflows, the edges
      of each cast), compared with Tessera on and off, and their
      expected output from the C version. Check: `make installcheck`;
      the diff of `test/expected/types.out` read line by line: every
      `ERROR` is the core's.

## 3. One reader and one loop

- [x] 3.1 Move `Source<T>` and the row loop of
      `tessera_kernels::calendar` into a module of their own, the loop
      generic over its error; the calendar uses them. Only lines move.
      Check: `cargo ab moved`; `cargo run --release --locked -p
      tessera-bench -- --base HEAD~1 --disasm --module kernels` shows
      the calendar's loops the same.
- [x] 3.2 Move the reader of a column-or-scalar argument from
      `crates/tessera-capi/src/c/calendar.rs` into `c/source.rs`. Only
      lines move. Check: as 3.1; `cargo test --locked -p tessera-capi
      --test calendar`.

## 4. The rules in Rust

- [x] 4.1 Write `tessera_kernels::float`: `FloatError` with the core's
      SQLSTATEs and messages, classified in `error.rs`; the comparison,
      the arithmetic of f32 and f64, the casts; the batch functions over
      `Source<T>`: compare (narrows the selection), arith, cast. Its
      tests: each rule against a reference written another way, over
      all pairs of the edge values the design lists and random bit
      patterns; the batch functions over NULLs, sparse and dense
      selections, more than one word, and the first error at a later
      row. Check: `cargo test --locked -p tessera-kernels float`.
- [x] 4.2 A Kani proof that a cast to an integer accepts exactly the
      floats whose rounded value is in range, and gives that value, for
      each source and target width, run by the Kani job of `ci.yml`.
      Check: `cargo kani --harness-timeout 600 -Z unstable-options -p
      tessera-kernels`; if it cannot finish, an exhaustive test around
      each bound instead, said in the pull request.

## 5. The entry points and the C side

- [x] 5.1 Write `include/tessera/float.h` (`TessFloatArg`,
      `TessFloatOp`, `TessFloatType`, the three calls) and
      `crates/tessera-capi/src/c/float.rs`, reading `f32` and `f64`
      from Datums. A test of every entry point through its raw structs:
      every operation, type pair and shape, NULLs and errors; the Miri
      job of `ci.yml` runs it. Check: `cargo test --locked -p
      tessera-capi --test float`; `cargo +nightly miri test --locked -p
      tessera-capi --test float`.
- [ ] 5.2 Make `kernels/float.c` call the entry points once a batch: it
      keeps the table of the functions, the call's checks and nothing
      else; no `ereport` is left in it. Update the paragraph on floats
      in `docs/function.md`. Check: `make installcheck` with
      `test/expected/types.out` unchanged.

## 6. The checks of the whole

- [ ] 6.1 Run the mutants of the changed Rust lines: none survives, or
      each one left is listed in `.cargo/mutants.toml` with the reason
      it is equivalent. Check: `cargo mutants --in-diff` against the
      branch's base, its `missed.txt` empty or listed.
- [ ] 6.2 Measure as the design's plan says, on an idle machine the
      person confirms. Check: `cargo ab measure --base <task 2.1>
      --family win --cases '^float_'` flags no case; the run's id in
      the pull request.
- [ ] 6.3 Two outside reviews of the pull request; each finding fixed,
      or answered in the pull request.

## 7. The close

- [ ] 7.1 Close the change: `openspec archive float-kernels-in-rust`
      and its folder deleted in the same commit; the decisions of the
      design written into `docs/function.md`; the roadmap entry
      removed, and an entry added for the float transitions of the
      aggregates. Check: `.github/scripts/check-specs.sh`.
