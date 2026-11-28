## 0. How the work goes

The branch `agg-folded-arguments` starts from `main` at the close of
`parquet-integer-columns`. The maintainer approves the proposal before
any code. The work goes in two pull requests into `main`, each with its
implementation and its count, reviewed by Codex and Claude Fable: the
first stage, groups 2 and 3; the second, groups 4 to 6, which closes the
change. Every task is one commit, except 6.2, the description; a commit
of more than about 500 lines of production code says why it is not
split. Each commit passes `.github/scripts/check-specs.sh` and the check
named in its task. "The suites" means `make && make install` and `make
installcheck EXTRA_REGRESS_OPTS="--temp-instance=$PWD/test/tmp_check"`
on PostgreSQL master built as CI builds it, with assertions and
injection points; CI runs 19 and 18.

## 1. The proposal

- [x] 1.1 Write the proposal, the specs, the design and the tasks of
      this change. Check: `openspec validate agg-folded-arguments
      --strict`; `.github/scripts/check-specs.sh`

## 2. Aggregates folded from shared states

- [x] 2.1 `bench/pg`: the family `aggfold`, which makes its own table,
      its cases and twins as the design's measurement plan lists them,
      and its entry in `bench/pg/README.md`; nothing of the node
      changes. Check: the family's queries run against a cluster of the
      base, their plans read
- [x] 2.2 The outputs apart from the states, with no change of
      behavior: the plan data carries the states and an output for each
      aggregate of the scan tuple, every output its own state; equal
      arguments with no volatile function share a computed column, and
      a test keeps two of `nextval`; the node keeps `values[]` for the
      states and fills the scan slot and the groups' batch from the
      outputs; the plan data's version raised. Check: the suites; `make
      crosscheck`
- [x] 2.3 The folded forms of integers for `sum`, `avg` and `count`,
      where the static ranges rule out every failure: the steps and
      their constants, the groups by `e`, `FILTER` and `DISTINCT` and
      their rule, ahead of the cap and the estimates; the shared
      states, the derived outputs in 128 bits, the costs of the states,
      `Folded Aggregates` in `EXPLAIN VERBOSE`; no folding in a partial
      or final aggregation. Tests of the design's "Tests" for these,
      with their spec marks. Check: the suites; `make crosscheck`
- [x] 2.4 The check of errors, and the forms it lets fold: the extremes
      of each folded `e` over the input, before a row spill, the
      evaluation of the argument at both when the input ends, their
      reset at a rescan; tests of each integer's range, nested forms,
      `WHERE`, `FILTER`, `HAVING`, `LIMIT`, a spill and a rescan. Check:
      the suites; `make crosscheck`
- [x] 2.5 `min` and `max` of folded forms of integers. Tests. Check: the
      suites
- [x] 2.6 Forms whose type is `numeric`: columns of tables of a declared
      precision and integers cast to `numeric`, the bounds of their
      steps and of the derived sum, `count(e)` beside the sum state of a
      `numeric` `e`, `sum`, `avg`, `count`, and `min` and `max` with a
      positive factor; tests with `NaN`, groups of `NaN` alone, at the
      bounds, and a function's column left unfolded. Check: the suites;
      `make crosscheck`
- [x] 2.7 `DISTINCT`: `sum`, `avg` and `count` of integers from the sum
      state of `avg(DISTINCT e)`, and `count` of `numeric` from
      `count(DISTINCT e)`; tests with a factor of 0, `NaN` and pairs past
      `hash_mem`. Check: the suites
- [x] 2.8 Aggregates of a constant, from `count(*)` with the
      aggregate's `FILTER`; tests over no rows and a `FILTER` that
      leaves none. Check: the suites

## 3. The count of the first stage

- [x] 3.1 Count as the design's measurement plan says, at the
      maintainer's word that the machine is idle: `aggfold` against 2.1,
      ClickBench's Q30, Q3, Q4 and the control against the same base in
      turns, with pg_duckdb's cycles beside, and the times of Q30. Keep
      the report under `docs/benchmarks/`; the first pull request's
      description follows. Check: `.github/scripts/check-specs.sh`

## 4. The words of flags

- [x] 4.1 `crates/tessera-kernels/src/accumulate.rs`,
      `crates/tessera-capi/src/c/table.rs`, `include/tessera/table.h`:
      `tess_table_combine` over `max(1, (n + 63) / 64)` words of flags,
      its check of the payload's size in place of the count, its decoded
      kinds off the stack's array of 64; the merge of 0, 63, 64, 65,
      127, 128 and 130 states, payloads exact and one word short, and a
      wrong kind past the 64th, in Rust and through the C API. Check:
      `make rust-check`; the machine code of the merge's loop before and
      after; the mutants of the diff; the suites
- [ ] 4.2 `nodes/`: every place the design's "The words of flags" lists,
      in words for each 64 states; `PairsEstimate.set_pairs` sized by
      the DISTINCT aggregates. Check: the suites
- [ ] 4.3 The cap lifted: the planner's and the node's checks of the
      count gone, the bounds of 1 664 computed columns and of a group's
      record within the first chunk in their place;
      `docs/limitations.md` without the cap. `test/sql/agg.sql` and
      `test/sql/parallel.sql`: 65 and 200 aggregates, grouped and not,
      generic and DISTINCT among them, past `hash_mem`, and groupings
      past each bound, with their spec marks. Check: the suites; `make
      crosscheck`

## 5. The count of the second stage

- [ ] 5.1 Count as the design's measurement plan says: the times of the
      cases of more than 64 aggregates, the counts of the cases of 64 or
      fewer of `aggfold`, Q3 and Q4, against the first stage's end. Keep
      the report under `docs/benchmarks/`. Check:
      `.github/scripts/check-specs.sh`

## 6. The close

- [ ] 6.1 `openspec archive agg-folded-arguments` writes the specs of
      `aggregate` and `hash-table`; `openspec/specs/aggregate/design.md`
      holds the design of the fold and of the count of aggregates, and
      `openspec/specs/hash-table/design.md` the words of flags;
      `docs/nodes.md` points to the capability; the roadmap loses the
      entry `agg-sum-plus-constant` and the finding of the cap, and gains
      what the first stage left out: dates and times, `numeric` without a
      precision, parameters, and the fold in parallel plans. The change's
      folder is deleted. Check: `openspec validate --all --strict`;
      `.github/scripts/check-specs.sh`
- [ ] 6.2 The second pull request's description, as CONTRIBUTING.md asks
      of a pull request that closes a change.
