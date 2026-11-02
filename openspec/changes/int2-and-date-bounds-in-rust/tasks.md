## 0. How the work goes

Each task of sections 1 to 5 and 7 is one commit on the branch
`int2-and-date-bounds-in-rust`, in the worktree
`~/git/tessera-calendar`. Section 6 lists the checks of the whole; they
make no commit of their own, and their evidence goes into the pull
request. The whole change, from the proposal to its close, is one pull
request; code starts after the proposal is approved. Each commit
passes `make rust-check` and `.github/scripts/check-specs.sh`, run with
the version of OpenSpec that `ci.yml` names; a commit that touches C
also passes `make && make install` and `make installcheck
EXTRA_REGRESS_OPTS="--temp-instance=$PWD/test/tmp_check"` on
PostgreSQL master built with assertions and injection points, as CI
builds it. A commit that adds a public type or function documents its
ownership, lifetime and errors. The check named in a task comes on top.

## 1. The proposal

- [x] 1.1 Write the proposal, the design and the tasks of this change,
      with `skip_specs: true`. Check: `openspec validate
      int2-and-date-bounds-in-rust --strict`;
      `.github/scripts/check-specs.sh`

## 2. What the C version answers

- [x] 2.1 Give `cargo ab measure` a deadline, `--deadline <seconds>`,
      one for the whole run, its builds included: each step of a run in
      a process group of its own, the group ended and the run failed at
      the deadline; `bench/pg/run.sh` sets
      `client_connection_check_interval` for its `psql`. A test of the
      tool with a step that outlives its deadline, and the option in
      `docs/measuring.md`. Check: `cargo test --locked -p tessera-ab`.
- [x] 2.2 Add the six cases of the design's measurement plan to the
      family `win` of `bench/pg`, with `TimeZone` set in the family's
      session and their lines in `bench/pg/README.md`, and the table
      `bench_tstz` to `bench/pg/setup.sql`. Check: `bench/pg/run.sh
      measure win` with `CASES='^(int2|date_ts|tstz)'` and
      `REPETITIONS=1` runs them, and `plans.txt` shows each function
      computed by Tessera's node.
- [ ] 2.3 Add to the suite `types` the rows the design's "Tests" lists
      that the C version already answers as the core does: the edges of
      a date against a timestamp; the edges of smallint arithmetic and
      of `int2(int4)` that do not mix two errors; for the time zone, a
      date past the last timestamp, the least date in a zone west of
      UTC, a midnight out of range, and a change of zone and back in one
      session. Each compared with Tessera on and off, with its inputs
      shown, and its expected output from the C version. Check: `make
      installcheck`; the diff of `test/expected/types.out` read line by
      line: every `ERROR` is the core's.

## 3. Smallint

- [ ] 3.1 In `tessera-kernels`: the range of a result as a type
      parameter of the driver of the integer arithmetic, the type's own
      for int4 and int8; the smallint check in every row-by-row step,
      the lane loops of a whole word among them; the smallint forms of
      the vector `+`, `-` and `*` in the module `simd`, which flag a
      result out of range; `ArithmeticError::SmallintOutOfRange`,
      classified in `error.rs`; the cast `int4_to_int2`; the smallint
      operations and the cast in the benchmark programs `arith_int32`
      and `cast_int32` beside the int4 ones. The tests of the design,
      each error checked exactly, on the vector and the row-by-row
      paths. Check: `cargo test --locked -p tessera-kernels`, which CI
      runs on x86-64 and on arm64; `cargo run --release --locked -p
      tessera-bench -- --base HEAD~1 --disasm --module kernels` shows
      the int4 and int8 loops the same; `tessera-bench` counts the
      programs, its report in the pull request.
- [ ] 3.2 The four entry points in `tessera-capi`, declared in
      `include/tessera/kernels.h`, with a test target of their own
      through the raw structs: every operation and shape, NULLs, the
      exact first error. The target joins the Miri command of `ci.yml`
      in this commit; `docs/kernels.md` lists the entry points. Check:
      `cargo test --locked -p tessera-capi`; `cargo +nightly miri test
      --locked -p tessera-capi --test <the target>`.
- [ ] 3.3 Make `kernels/int.c` call them; `smallint_range` goes. Add to
      `types` the table of the proposal (row 2 `-32768 / -1`, row 5 a
      division by zero), run with Tessera on and off, both errors in the
      expected output, 22003 for each. Update the paragraph on smallint
      in `docs/function.md` and the comment at the head of
      `kernels/int.c`. Check: `make installcheck`; the diff of
      `types.out` is the new case alone; the same case on the base gives
      22012 with Tessera on, said in the pull request.

## 4. A date against a timestamp

- [ ] 4.1 In `tessera_kernels::calendar`: the bound of a date against a
      timestamp over `split`, its rule over the day and the time a
      function of its own; the tests of decision 2 and of the bound;
      the Kani proof of the rule, with covers of each operator's two
      outcomes, run by the Kani job of `ci.yml`, its module excluded in
      `.cargo/mutants.toml` as the float proofs are. Check: `cargo test
      --locked -p tessera-kernels calendar`; `cargo kani
      --harness-timeout 600 -Z unstable-options -p tessera-kernels`.
- [ ] 4.2 The entry point `tess_date_timestamp_filter`, declared in
      `include/tessera/calendar.h`, with a test target under Miri that
      joins the Miri command of `ci.yml`; `date_timestamp_evaluate` in
      `kernels/int.c` calls it and keeps only the call's checks. Update
      `docs/function.md`, the list of `docs/kernels.md` and the comment
      at the head of `kernels/int.c`. Check: `make installcheck` with
      `types.out` unchanged; the Miri test.

## 5. The time zone

- [ ] 5.1 The module `tessera_kernels::calendar::zone`: the view of the
      cache's words, learning a span, a second and a midnight, the
      reset, and the calls of the design over a batch, each sorting its
      rows into ready, unknown and for the core and giving the
      questions, and settling the unknown rows from the answers. The
      property tests over model zones the design lists, with a host that
      skips as C does and one that does not, and the check that each
      round settles its first question. Check: `cargo test --locked -p
      tessera-kernels zone`.
- [ ] 5.2 `TessZoneCache` and the entry points of the module in
      `include/tessera/calendar.h` and `tessera-capi`; the count of
      words in the layout probe of `kernels_layout.h`; a test target
      through the raw structs, the calls over one value among them, that
      joins the Miri command of `ci.yml`. Check: `cargo test --locked -p
      tessera-capi`; the Miri test.
- [ ] 5.3 Make `kernels/date.c` use them: the loops that ask the time
      zone library and teach the cache, with an injection point in the
      loop of spans; the reset on a change of zone; the statics and the
      functions the design's decision 5 lists go. A suite of its own,
      with the variant for builds without injection points as
      `spill_damage` has, forces the failure of `pg_next_dst_boundary`
      through a test callback and compares with the core. Update the
      paragraph on the time zone in `docs/function.md` and in
      `docs/kernels.md`, and the comment at the head of
      `kernels/date.c`. Check: `make installcheck` with `types.out`
      unchanged byte for byte.

## 6. The checks of the whole

- [ ] 6.1 The mutants of the changed Rust lines: none survives, or each
      one left is listed in `.cargo/mutants.toml` with the reason it is
      equivalent; a survivor that is not gets a test. Check: `git diff
      <base> > diff.txt`; `cargo mutants --in-diff diff.txt`, its
      `missed.txt` empty or listed.
- [ ] 6.2 Measure as the design's plan says, on an idle machine the
      person confirms. Check: the command of the plan flags no case
      slower; the runs' ids in the pull request.
- [ ] 6.3 Outside reviews of the work by Codex and Fable 5.1 as it goes;
      each finding fixed, or answered in the pull request.

## 7. The close

- [ ] 7.1 Close the change: `openspec archive
      int2-and-date-bounds-in-rust` and its folder deleted in the same
      commit; the decisions of the design written into
      `docs/function.md`; the roadmap entry removed, and the findings
      without a decision added to its "Not placed". Check:
      `.github/scripts/check-specs.sh`.
