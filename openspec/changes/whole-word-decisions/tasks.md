## 0. How the work goes

Every task is one commit on the branch `whole-word-decisions`, but
the opening of a pull request, which is a step of the work. The
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

- [x] 2.1 Cases of 8, 12, 16, 24 and 32 selected rows a word, at
      positions drawn anew in the form of `bench-unlearned-inputs`, in
      the reading fixtures of both widths, the matrices of the filters
      and of the comparisons, and `set_int32`. Check: `cargo test
      --locked -p tessera-capi --benches`; `tests/benchmark.rs` names
      the cases; `tessera-bench --base WORKTREE` over the programs that
      gained cases, as root and on the M5 Pro, for their noise
- [ ] 2.2 The rows build and the whole build of the design, on
      branches never merged, measured against each other over the
      density cases and full words of every program, twice on each
      machine; the transitions of each family, storage and key count,
      with the ids of the runs and the instructions that show each
      path, in the design. Check: `.github/scripts/check-specs.sh`
- [ ] 2.3 The bound of each family, a constant beside its driver, one
      for each architecture where the machines part. Check: the
      listings; the counters of each family's program at and around
      its bound
- [ ] 2.4 The sets: the least rows from which a word pays whole, from
      a table of the measured key counts, in place of `WORD_KEYS`.
      Check: the listings; the counters of `set_int32`
- [ ] 2.5 The sets count present rows, and skip a word without one.
      Check: `cargo test --locked -p tessera-kernels`; the counters of
      `set_int32`, with `nulls-all`, `nulls-mixed` and `nulls-random`
- [ ] 2.6 The families that decide on one word, on
      `nulls-all`: present rows where a whole word of NULL rows costs
      more than its rows. Check: the counters of each family changed
- [ ] 2.7 Open the first pull request. Check: every item of its
      description points to a test, a command or the id of a run

## 3. The place of the decision on x86-64

- [ ] 3.1 The control build of the design, on a branch never merged,
      against the base, over the cases that go by rows: what the
      vector path costs them, with the ids of the runs, in the design.
      Check: `.github/scripts/check-specs.sh`
- [ ] 3.2 The forms of the design for the drivers, each a hypothesis
      written down first, until the row cases are within the noise of
      the control or the search ends. Check: the listings; the counters
      of every program whose code changes, against the base and the
      control
- [ ] 3.3 The loops of `count` and of the aggregates: each shape on the
      other machine. Check: the listings; the counters of
      `aggregate_int32` and `aggregate_int64`
- [ ] 3.4 `as_chunks` in `word_slots`. Check: the listings; the
      counters of every program whose code changes
- [ ] 3.5 Open the second pull request. Check: as in 2.7

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
      roadmap; the runs cited are copied to `docs/benchmarks/`; a
      finding without a decision goes to "Not placed".
      Check: `.github/scripts/check-specs.sh`; no line of the roadmap
      names `whole-word-decisions`
- [ ] 4.4 Open the third pull request, with the outcome under Decision
      as `CONTRIBUTING.md`, "Closing a change", asks. Check: as in 2.7
