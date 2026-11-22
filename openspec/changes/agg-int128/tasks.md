## 0. How the work goes

Every task is one commit on the branch `agg-int128`, but the opening of
a pull request, which is a step of the work. Each commit passes `make
rust-check` and `.github/scripts/check-specs.sh`, run with the version
of OpenSpec that `ci.yml` names; the check named in a task comes on
top. A run that loads the machine waits for the maintainer's word, as
`docs/measuring.md` says, and takes five minutes at most. The work
starts when the maintainer has approved this proposal. The proposal is
one pull request; the work and the close are a second.

## 1. The proposal

- [x] 1.1 Write the proposal, the design and the tasks of this change,
      with `skip_specs: true`, and its entry in the roadmap, first in
      the queue. Check: `openspec validate agg-int128 --strict`;
      `.github/scripts/check-specs.sh`

## 2. The cases

- [x] 2.1 `bench/pg/anyagg.sql` makes the table `bench_digits` once and
      gets the eight cases of the design's measurement plan, named in
      both lists of its plans; the family's entry in
      `bench/pg/README.md` names them. Check: `cargo ab measure --base
      HEAD --family anyagg --cases digits --repetitions 3 --rounds 1
      --deadline 300`, its `plans.txt` showing a TessAgg in each case

## 3. The terms

- [x] 3.1 Every check of the sum's magnitude against its bound by `abs`
      uses `unsigned_abs`: in `Sum::add`, `Sum::add_many`, the checks
      of `decimal::sum_terms` and `decimal::merge_partials` on the way
      in, and `state_partial`; the fast path of `SumState::add_to`
      stays. Tests: a state of `i128::MIN` refused on the way in, from
      C and from bytes. Check: `cargo test --locked -p tessera-kernels
      -p tessera-capi`
- [x] 3.2 `Integers` hands over every non-NULL value in bulk, and the
      promise of `Terms::fold_decimals` and `DecimalWord` says so;
      `Term` gets the variant of an integer past 18 digits, which
      `Integers::term` gives and every place the design lists takes;
      the comments of `tess_decimal_sum` and
      `tess_table_accumulate_sums` and of the node's sum say which rows
      go to the rest. The Rust tests of the design's list, the models
      of the kernels' tests too; the proofs by Kani of the sum's
      arithmetic. Check: `cargo test --locked -p tessera-kernels -p
      tessera-capi`; `cargo kani -p tessera-kernels` over the new
      proofs, each within the limit CI gives; `cargo mutants` over the
      changed lines of both crates, by their own tests; `tessera-bench
      --base <the commit of 2.1> --disasm --module kernels`, then
      `--disasm --bench <program>` for each program
- [x] 3.3 `test/sql/agg.sql` sums `bigint` values at the edges of
      `int64` with and without groups, with many groups, with workers
      and past `hash_mem`; its comments and those of
      `test/sql/parallel.sql` no longer call a `bigint` of 19 digits a
      rest; `docs/nodes.md`, `docs/kernels.md` and `docs/table.md` say
      that a sum takes every integer. Check: `make installcheck
      EXTRA_REGRESS_OPTS="--temp-instance=$PWD/test/tmp_check"`, the
      diffs of `test/expected/agg.out` and `parallel.out` read line by
      line

## 4. The measurements

- [ ] 4.1 The counts and the times of the design's measurement plan,
      the base the commit of 2.1, each hypothesis judged as written
      there, the ids of the runs and their results in the design.
      Check: `cargo ab measure --base <the commit of 2.1> --family
      anyagg --counters --deadline 300`; `cargo ab measure --base <the
      commit of 2.1> --family anyagg --cases digits --rounds 1
      --deadline 300`; `.github/scripts/check-specs.sh`

## 5. The close

- [ ] 5.1 Close the change: delete its folder; the entry leaves the
      roadmap; the runs cited are copied to `docs/benchmarks/`; the
      decisions of the design that outlive the change go into
      `docs/nodes.md` and `docs/kernels.md`; a finding without a
      decision goes to "Not placed". Check:
      `.github/scripts/check-specs.sh`; no line of the roadmap names
      `agg-int128`
- [ ] 5.2 Open the pull request of the work, with the outcome under
      Decision as `CONTRIBUTING.md`, "Closing a change", asks. Check:
      every item of its description points to a test, a command or the
      id of a run
