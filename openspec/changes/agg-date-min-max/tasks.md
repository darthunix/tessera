## 0. How the work goes

The branch `agg-date-min-max` starts from `main` at the close of
`parquet-page-headers`. The maintainer approves the proposal before
any code. The work goes in one pull request into `main`, reviewed by
Codex and Claude Fable. Every task is one commit, except 4.3, the pull
request's description. Each commit passes
`.github/scripts/check-specs.sh` and the check named in its task. "The
suites" means `make && make install` and `make installcheck
EXTRA_REGRESS_OPTS="--temp-instance=$PWD/test/tmp_check"` on PostgreSQL
master built as CI builds it, with assertions and injection points; CI
runs 19 and 18. A run that loads the machine waits for the
maintainer's word and takes five minutes at most.

## 1. The proposal

- [x] 1.1 Write the proposal, the design, the tasks and the change of
      the spec `hash-table` of this change. Check: `openspec validate
      agg-date-min-max --strict`; `.github/scripts/check-specs.sh`

## 2. The cases

- [x] 2.1 `bench/pg/setup.sql` makes `bench_clock` and `bench_minutes`;
      `bench/pg/anyagg.sql` and `bench/pg/wordkey.sql` get the cases of
      the design's measurement plan; the families' entries in
      `bench/pg/README.md` name them. Check: `cargo ab measure --base
      HEAD --family anyagg --cases 'minmax|bool_and' --repetitions 3
      --rounds 1 --deadline 300` and `cargo ab measure --base HEAD
      --family wordkey --cases 'time|money|clock' --repetitions 3
      --rounds 1 --deadline 300`, their `plans.txt` showing the plans
      of each case

## 3. The work

- [x] 3.1 The extremes by the kernels: `kernels/int.c` registers `min`
      and `max` of `smallint` and `date`, `bool_and`, `bool_or` and
      `every` with the codes of `integer`, and `min` and `max` of
      `timestamp` and `timestamptz` with those of `bigint`;
      `aggregate_kind` and `batch_aggregate` in `nodes/agg_planner.c`
      take them; `nodes/agg.c` reads the width of an extreme by the
      rule of the word keys. Tests, as the design's "Tests" lists them
      for the extremes, in `test/sql/agg.sql` and
      `test/sql/parallel.sql`; `docs/nodes.md` and `docs/function.md`
      name the new aggregates. Check: the suites, the diffs of the
      expected outputs read line by line; `make crosscheck`
- [x] 3.2 `time` and `money` as integers: `tess_word_key_kind` gives
      both the kind of eight bytes; `kernels/int.c` registers their
      comparisons with those of `bigint` and their extremes;
      `word_equality` in `nodes/join_planner.c` takes the equality of
      `time`; the sets of `IN` in `runtime/expr.c` take both. The
      section "Keys of other types of 8 bytes" of `test/sql/types.sql`
      becomes "Keys of time and money in a word" and "Keys left to the
      core", which the spec's two pending scenarios then name; tests of
      the comparisons, the lists of `IN`, a sort and the extremes of
      both. The changed requirement goes into
      `openspec/specs/hash-table/spec.md` in this commit, since the
      spec must say what the code does and its tests' names change.
      `openspec/specs/hash-table/design.md`, `docs/nodes.md` and
      `docs/function.md` say the types are words. Check: the suites,
      the diffs read line by line; `make crosscheck`
- [x] 3.3 `DISTINCT` in an extreme: one function in
      `nodes/agg_planner.c` says whether an aggregate's `DISTINCT`
      matters, false for an extreme of the node's own; every place in
      `nodes/agg_planner.c` and `nodes/agg.c` that reads `aggdistinct`
      asks it. Tests, as the design's "Tests" lists them for
      `DISTINCT`, in `test/sql/agg.sql`; `docs/nodes.md` and
      `docs/limitations.md` say which `DISTINCT` aggregates the node
      takes. Check: the suites, the diffs read line by line; `make
      crosscheck`

## 4. The count and the close

- [x] 4.1 Count as the design's measurement plan says, the base the
      commit of 2.1, each hypothesis judged as written there; keep the
      report under `docs/benchmarks/`. Check: `cargo ab measure --base
      <the commit of 2.1> --family anyagg --cases 'minmax|bool_and'
      --counters --cycles --rounds 2 --deadline 300`, the same for
      `wordkey` with `--cases 'time|money|clock|key_date_group'`;
      `cargo clickbench run --counters --tries 5 --mode on --workers 0`
      for Q7, Q1 and Q3; `.github/scripts/check-specs.sh`
- [ ] 4.2 Close the change: `openspec archive agg-date-min-max` finds
      the requirement in `openspec/specs/hash-table/spec.md` as 3.2 put
      it, and the folder it makes under `openspec/changes/archive/` is
      deleted;
      what was decided goes into `docs/nodes.md` and the design of
      `hash-table`; the entry `agg-date-min-max` leaves the roadmap,
      the entries `types-outside-tpch` and `table-key-types` no longer
      name `time`, and a finding without a decision goes to the list
      "Not placed". Check: `openspec validate --all --strict`;
      `.github/scripts/check-specs.sh`; no line of the roadmap names
      `agg-date-min-max`
- [ ] 4.3 The pull request's description, as `CONTRIBUTING.md` asks of
      a pull request that closes a change. Check: every item of its
      description points to a test, a command or the id of a run
