## 0. How the work goes

The branch `parquet-string-views` starts from `main`. The maintainer
approved the proposal on 2026-11-24, before any code. The work goes in
one pull request into `main`, reviewed by Codex and Claude Fable. Every
task is one commit, except 5.3, the pull request's description. Each
commit passes `.github/scripts/check-specs.sh` and the check named in
its task. It names, in a scenario's "Verified by", each test it lands.
"The suites" means `make && make install` and `make installcheck
EXTRA_REGRESS_OPTS="--temp-instance=$PWD/test/tmp_check"` on PostgreSQL
master built as CI builds it, with assertions and injection points; CI
runs 19 and 18. A count is taken by `cargo clickbench run --counters`
and judged by its instructions, as the design's measurement plan says. A
commit that changes a hot path carries in its message its counts against
the commit before it.

## 1. The proposal

- [x] 1.1 Write the proposal, the spec deltas, the design and the
      tasks of this change. Check: `openspec validate
      parquet-string-views --strict`; `.github/scripts/check-specs.sh`

## 2. `datum-arrow` reads views

- [x] 2.1 `crates/datum-arrow`: `Utf8View` and `BinaryView` in
      `ArrowType`, a variant of `Values` for a view array, its views
      and its buffers, with the drawing of a view; the read plans of
      the pairs of `LargeUtf8` and `LargeBinary`; every long view
      asked for checked against its buffers before its bytes are read;
      the broken shapes in `ReadPlan::read`'s list; `public-api.txt`.
      Tests: the pairs, views read against their twins by a property
      test, views of an array of `arrow-array`, the broken shapes, no
      panic on any input. The scenarios of "Views of strings" name
      them. Check: `make rust-check`; `cargo public-api -p datum-arrow
      -sss` against `public-api.txt`

## 3. The reader reads views

- [x] 3.1 `crates/tessera-parquet`: the plain view of a column of bytes
      asks for `BinaryView`; the reader's input of a view array, with
      the slices of its buffers in a place its caller gives; a plan of
      its own for a dictionary's values; the clauses' strings through
      views; the bytes form of a window of views, each row's pointer
      into its page or its view, and the window's cheaper check value
      by value, which replaces the check in one pass and its proof.
      Tests: a chunk that falls back from its dictionary read as views,
      with NULLs and values of 0, 12, 13 and a thousand bytes; the
      clauses over views; the check of views against each value's, by
      a property test; the cheaper check against the plan's row by row,
      under Miri as the bytes form's tests are; in
      `test/sql/parquet.sql` the scenario "A chunk that leaves its
      dictionary", against a heap table. The message carries the counts
      of Q26 and Q28 against 2.1. Check: `make rust-check`; the suites;
      `make crosscheck`
- [ ] 3.2 The contracts: `parquet-fdw`'s forecast with the clauses,
      `datum-arrow`'s with the input of a column. In the roadmap, the
      deviation over the clauses' boundary in `parquet-reader-boundary`
      and a new entry for simplifying `datum-arrow`. Check:
      `.github/scripts/check-specs.sh`

## 4. The count

- [ ] 4.1 Count as the design's measurement plan says, against the
      commit the work starts from, with pg_duckdb's cycles beside. Keep
      the report under `docs/benchmarks/`. Check:
      `.github/scripts/check-specs.sh`

## 5. The close

- [ ] 5.1 Each scenario's "Verified by" names its test; none says
      pending. Check: `.github/scripts/check-specs.sh`
- [ ] 5.2 The designs of `datum-arrow` and `parquet-fdw` take the
      decisions of this design. `openspec archive parquet-string-views`,
      its folder under `openspec/changes/archive/` deleted in the same
      commit. In the roadmap, the entry `parquet-string-pages` removed
      and what is left added. Check: `openspec validate --strict
      --all`; `.github/scripts/check-specs.sh`
- [ ] 5.3 The pull request's description: what was done, the counts
      with the ids of the runs, where the work departed from the design
      and why, the findings, and what is left.
