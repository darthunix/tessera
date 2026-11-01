## 0. How the work goes

Every task is one commit on the branch `bench-unlearned-inputs`, but
the opening of the pull request, which is a step of the work. Each
commit passes `make rust-check` and `.github/scripts/check-specs.sh`,
run with the version of OpenSpec that `ci.yml` names; the check named
in a task comes on top. "As root" is a command the maintainer types on
the Intel Mac, after `sudo -v`, on an idle machine on mains power; the
agent asks first. "On the M5 Pro" is a run the maintainer makes there
and relays.

## 1. The proposal

- [x] 1.1 Write the proposal, the design and the tasks of this change,
      with `skip_specs: true`, and its entry in the roadmap. Check:
      `openspec validate bench-unlearned-inputs --strict`;
      `.github/scripts/check-specs.sh`

## 2. The pilot

- [x] 2.1 The permuted inputs of the fixture, the model of the misses
      of a case, and the period of a ring in the runner. Check: `cargo
      test --locked -p tessera-capi --benches`; a test that every input
      of a case holds the same words, in another order
- [x] 2.2 The three forms of the design and their controls on the two
      cases of the pilot, each a case of its own name, so that the runs
      can filter them; `K` a constant of the case. Check: `cargo test
      --locked -p tessera-capi --benches`, every input checked against
      its model; `cargo bench --locked -p tessera-capi --bench <program>
      -- --list` names every form and control with its reference
- [ ] 2.3 The runs of the pilot, as root and on the M5 Pro, twice each,
      each program against itself, with the hypothesis
      of each form written down first; the form chosen by the criterion
      of the design, and the others removed. A pilot where no form
      passes stops the work for the maintainer. Check: the ids of the
      runs in the commit and in the design

## 3. The cases

- [ ] 3.1 `random` and `nulls-random` of both widths' reading fixtures
      and `shuffled` of `set_int32` take the chosen form. Check:
      `cargo test --locked -p tessera-capi --benches`;
      `tests/benchmark.rs` names the cases
- [ ] 3.2 The README of the benchmarks and `docs/measuring.md` say
      which cases the predictor learns and which it cannot, and where
      the inputs of a case lie. Check: `.github/scripts/check-specs.sh`
- [ ] 3.3 `tessera-bench --base WORKTREE` over every program that has
      the new cases, as root and on the M5 Pro: no FAIL and no
      UNSTABLE on them; their spread, MODES and misses a call go into
      the design, with the ids of the runs. A FAIL or an UNSTABLE stops
      the work for the maintainer. Check:
      `.github/scripts/check-specs.sh`

## 4. The close

- [ ] 4.1 Close the change: delete its folder; the entry leaves the
      roadmap; what was decided goes into the README of the
      benchmarks; the runs cited are copied to `docs/benchmarks/`, as
      `docs/measuring.md` says; a finding without a decision goes to
      "Not placed".
      Check: `.github/scripts/check-specs.sh`; no line of the roadmap
      names `bench-unlearned-inputs`
- [ ] 4.2 Open the pull request, with the outcome under Decision as
      `CONTRIBUTING.md`, "Closing a change", asks. Check: every item of
      its description points to a test, a command or the id of a run
