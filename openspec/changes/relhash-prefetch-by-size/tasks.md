## 0. How the work goes

Every task is one commit on the branch `relhash-prefetch-by-size`.
Each commit passes `make rust-check` and
`.github/scripts/check-specs.sh`, run with the version of OpenSpec
that `ci.yml` names; the check named in a task comes on top. "The
counters" are, as root on the Intel Mac and on the M5 Pro,
`tessera-bench --base <base> --candidate <branch> --bench <program>`,
twice on each machine, judged by the rule of the design's measurement
plan; the agent asks first, the maintainer runs and relays.

## 1. The proposal

- [x] 1.1 Write the proposal, the design and the tasks of this change,
      with `skip_specs: true`, and its entry in the roadmap. Check:
      `openspec validate relhash-prefetch-by-size --strict`;
      `.github/scripts/check-specs.sh`

## 2. The sizes

- [ ] 2.1 `table_large` builds tables of about 256 KiB, 2 MiB, 16 MiB
      and 160 MiB, each a case of its own probed from its own ring of
      batches; the README entry says so. Check: `cargo test --locked
      -p tessera-capi --benches`; `tessera-bench --base WORKTREE
      --bench table_large`, as root and on the M5 Pro, with no FAIL and
      no UNSTABLE

## 3. The bound

- [ ] 3.1 The hint of x86-64, `prefetcht0`, in `table/lanes.rs`, given
      by every probe, as a candidate. Check: `cargo test --locked -p
      relhash`; `.github/scripts/check-trust.sh`; the counters of
      `table_int32` and `table_large` against the base, on the Intel
      Mac; on the M5 Pro the base's hint against a candidate that gives
      none, over the same programs
- [ ] 3.2 The bound of each architecture read off those runs, as the
      design says, into the design. Check: the ids of the runs

## 4. The choice

- [ ] 4.1 The probe and the insert of a batch choose the hint once a
      call by the table's size, through a constant parameter of their
      vertical loop; `PREFETCH_MIN_BYTES` for each architecture beside
      the hint. Check: `cargo test --locked -p relhash`; `cargo kani -p
      relhash` where it runs, or the proofs' build; the counters of
      `table_int32` and `table_large`; the rule "No cost to Tessera":
      `tessera-bench --base HEAD~1 --disasm` over every program on both
      targets, and `--module` on the M5 Pro
- [ ] 4.2 The design of `relhash` says how the hint is chosen and what
      it gains and costs on each machine; the forecast of its contract
      says what the change touched. Check:
      `.github/scripts/check-specs.sh`

## 5. The close

- [ ] 5.1 Close the change: delete its folder; the entry leaves the
      roadmap; a finding without a decision goes to "Not placed".
      Check: `.github/scripts/check-specs.sh`; no line of the roadmap
      names `relhash-prefetch-by-size`
- [ ] 5.2 Open the pull request, with the outcome under Decision as
      `CONTRIBUTING.md`, "Closing a change", asks, and the parts it
      touched against the forecast under "Architecture". Check: every
      item of its description points to a test, a command or the id
      of a run
