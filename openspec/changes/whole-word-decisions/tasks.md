## 0. How the work goes

Every task is one commit on the branch `whole-word-decisions`. The
sections reach `main` in three pull requests, as the design's "Steps"
says. Each commit passes `make rust-check` and
`.github/scripts/check-specs.sh`, run with the version of OpenSpec
that `ci.yml` names; the check named in a task comes on top. The work
starts after `bench-unlearned-inputs` is closed, and every base of a
run is after it.

The words used below:

- **"The listings"**: `cargo run --release --locked -p tessera-bench
  -- --base HEAD~1 --disasm`, over all programs, and the same with
  `--target aarch64-apple-darwin`.
- **"The counters"**: as root on the Intel Mac and on the M5 Pro,
  `tessera-bench --base <base> --candidate <branch> --bench
  <program>`, twice on each machine where the candidate's code
  changes, judged by the rule of the design's measurement plan, with
  the runs' directories in the commit and the pull request. The agent
  asks first; the maintainer runs and relays.

## 1. The proposal

- [x] 1.1 Write the proposal, the design and the tasks of this change,
      with `skip_specs: true`, and its entry in the roadmap, which
      takes `x86-row-paths` and the four findings. Check: `openspec
      validate whole-word-decisions --strict`;
      `.github/scripts/check-specs.sh`

## 2. The bounds

- [ ] 2.1 Cases of 12, 16 and 24 selected rows a word in the reading
      fixtures of both widths, and in `set_int32` the densities and
      key counts the model needs. Check: `cargo test --locked -p
      tessera-capi --benches`; `tests/benchmark.rs` names the cases
- [ ] 2.2 The model: runs of every program with the base on both
      sides, twice on each machine; the fit of each family and of the
      sets, with its residuals, in the design. Check: the ids of the
      runs
- [ ] 2.3 The bound of each family, a constant beside its driver, one
      for each architecture where the machines part. Check: the
      listings; the counters of each family's program at and around
      its bound
- [ ] 2.4 The sets: the least rows from which a word pays whole, from
      the number of keys, in place of `WORD_KEYS`. Check: the
      listings; the counters of `set_int32`
- [ ] 2.5 The sets count present rows, and skip a word without one.
      Check: `cargo test --locked -p tessera-kernels`; the counters of
      `set_int32`, with `nulls-all`, `nulls-mixed` and `nulls-random`
- [ ] 2.6 The families that decide on their first word, on
      `nulls-all`: present rows where a whole word of NULL rows costs
      more than its rows. Check: the counters of each family changed
- [ ] 2.7 Open the first pull request. Check: every item of its
      description points to a test, a command or the id of a run

## 3. The place of the decision on x86-64

- [ ] 3.1 The forms of the design for the drivers, each a hypothesis
      written down first, until the row paths cost what they did
      before AVX2 or the search ends. Check: the listings; the
      counters of every program whose code changes
- [ ] 3.2 The loops of `count` and of the aggregates: each shape on the
      other machine. Check: the listings; the counters of
      `aggregate_int32` and `aggregate_int64`
- [ ] 3.3 `as_chunks` in `word_slots`. Check: the listings; the
      counters of every program whose code changes
- [ ] 3.4 Open the second pull request. Check: as in 2.7

## 4. The fold of a full word, and the close

- [ ] 4.1 The forms of the design for `try_fold_words` on x86-64, each
      a hypothesis written down first. Check: the listings; the
      counters of `column_reader`, `aggregate_int32` and
      `aggregate_int64`
- [ ] 4.2 Write what this design decided, and why, into the
      documentation of the drivers, of `simd` and of `tessera-capi`,
      and into `docs/kernels.md`. Check: `RUSTDOCFLAGS=-Dwarnings cargo
      doc --locked -p tessera-kernels --no-deps`
- [ ] 4.3 Close the change: delete its folder; the entry leaves the
      roadmap; a finding without a decision goes to "Not placed".
      Check: `.github/scripts/check-specs.sh`; no line of the roadmap
      names `whole-word-decisions`
- [ ] 4.4 Open the third pull request, with the outcome under Decision
      as `CONTRIBUTING.md`, "Closing a change", asks. Check: as in 2.7
