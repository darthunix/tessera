## 0. How the work goes

The branch `parquet-fdw` starts from `source-scan`'s branch, and its
pull request is based on it until `source-scan` is merged; the crate
`datum-arrow` is on `main` (`openspec/specs/datum-arrow/`). Every task
is one commit, reviewed by Codex and Claude Fable before the next
starts. Each commit passes `.github/scripts/check-specs.sh` and the
check named in its task. "The suites" means `make && make install` and
`make installcheck
EXTRA_REGRESS_OPTS="--temp-instance=$PWD/test/tmp_check"` on PostgreSQL
master, 19 and 18 built as CI builds them. A measurement waits for the
maintainer to say that the machine is idle.

## 1. The proposal

- [x] 1.1 Write the proposal, the spec delta, the design, the tasks and
      the contract of this change; in the roadmap, the entry
      `parquet-fdw` taken, and the new entries `text-keys-by-bytes` and
      `clickbench-entry` after it. Check: `openspec validate parquet-fdw
      --strict`; `.github/scripts/check-specs.sh`

## 2. The reader

- [ ] 2.1 `crates/tessera-parquet`: a file's footer, its schema and
      groups of rows, their least and greatest values and `NULL` counts
      with Parquet's rules of trust, the read plans of `datum-arrow` for
      the columns, a window of a column read for a selection, a chunk's
      dictionary page and its plain values decoded by the reader's own
      code, from a page headed `PLAIN` or `PLAIN_DICTIONARY`, tested
      against the crate's reader of the same chunk, and the
      C entry points with their statuses and caught panics; a writer of
      test files behind a feature of its own, with footers rewritten to
      the old rule; unit tests over files the writer makes, among them a
      column marked as a string with bytes that are not UTF-8 read under
      a selection that leaves them out; the crate in the Rust jobs of CI
      and in its mutants. Check: `make rust-check`; `cargo mutants` over
      the crate's lines

## 3. The wrapper

- [ ] 3.1 The extension `tessera_parquet` in `parquet/`: the wrapper,
      its handler and validator, the options and their privileges, the
      files and patterns, the footers kept by path, size, time, device
      and inode, the mapping of columns checked at planning and when a
      scan begins; the test module that
      writes the suite's files; the suite `test/sql/parquet.sql` with
      the cases of the options and the columns refused. Check: the
      suites
- [ ] 3.2 The wrapper's rows: the scan, the values of every mapped type
      through `datum-arrow`, the check of UTF-8, `NULL`, rescans, files
      replaced and damaged, a cancel within a group seen through an
      injection point at each window read. Check: the suites;
      the cases of the types, of text from bytes and of reading rows

## 4. The source, its parallel reading and the first measurement

- [ ] 4.1 The source and `TessParquetScan`: windows read for a
      selection, values made through `datum-arrow` on request for the
      rows asked and kept until the batch is released, a count that
      reads no column, the lines of `EXPLAIN`, the duties of the
      wrapper's scan kept by the node. Check: the suites; the cases of
      batch parents, of the columns made for the rows kept, of one
      column asked again and of `EXPLAIN`, and the source's halves of
      the cases of text from bytes, of the length of a varchar and of
      reading rows, those without a clause
- [ ] 4.2 Parallel reading: the leader's list of files and groups, with
      each file's size, time, device and inode, checked by every
      participant on the file it opens, and the shared counter for the
      wrapper's and the source's partial paths, the counts of `EXPLAIN`
      over the participants. Check: the suites; the cases of one file in
      parallel and of a file replaced between participants
- [ ] 4.3 Estimates from the footers, the costs of both paths, and
      `ANALYZE`. Check: the suites; the cases of the rows and of the
      source chosen
- [ ] 4.4 The installed set builds the module from copies against the
      installed headers and runtime library. Check: `make -C
      test/installed installcheck`
- [ ] 4.5 `tools/tessera-clickbench` (`cargo clickbench`) with
      ClickBench's queries in `bench/clickbench/` and its README: the
      cluster, the foreign table over one file or a list, the modes
      Tessera on, Tessera off and pg_duckdb, the answers checked between
      them by companion queries where a `LIMIT` leaves the rows open,
      three tries a query, the report. Check: `make rust-check`; `cargo
      clickbench check` over the ten files
- [ ] 4.6 Stage 1, parallel groups of rows: Q1 to Q7 by 0, 2, 4 and 8
      workers, over the ten files and over `hits.parquet`, the run kept
      in `docs/benchmarks/`. Check: the run's report

## 5. The clauses on integers, dates and times

- [ ] 5.1 The clauses on integers, dates and times checked on stored
      values through `datum-arrow`'s bounds, `IN`, `IS NULL`, with the
      values of parameters and subqueries. Check: the suites; the cases
      of the clauses checked and left, and of values out of range, and
      the source's halves under a clause of the cases of text from bytes
      and of the length of a varchar
- [ ] 5.2 The groups of rows skipped by their least and greatest values,
      their `NULL` counts and their dictionaries. Check: the suites; the
      cases of the groups skipped and of the groups the footer cannot
      rule out
- [ ] 5.3 Stage 2, the 17 queries that read no string. Check: the run's
      report

## 6. The clauses on strings

- [ ] 6.1 Equality, `<> ''` and `LIKE` of fixed parts on stored bytes
      and on dictionaries. Check: the suites; the cases of the clauses
      checked and left, and the items on strings of the groups skipped
- [ ] 6.2 Stage 3, the 18 queries that filter on a string. Check: the
      run's report
- [ ] 6.3 Stage 4, the 15 queries that group by a string, as they are:
      the base of `text-keys-by-bytes`. Check: the run's report

## 7. The guides

- [ ] 7.1 `docs/parquet.md`, the guide of the wrapper: creating a table,
      the types and options, what the source checks and skips, parallel
      reading, `EXPLAIN`; the README and `docs/limitations.md`. Check:
      `.github/scripts/check-specs.sh`

## 8. The spec, the design and the contract

- [ ] 8.1 Every scenario of the spec delta names its test, and every
      pending check of the contract its check. Check: `openspec validate
      parquet-fdw --strict`; `.github/scripts/check-specs.sh`
- [ ] 8.2 `openspec/specs/parquet-fdw/design.md`, written for a reader
      who knows nothing of the part; `## Files` and `## Tests` last.
      Check: `.github/scripts/check-specs.sh`
- [ ] 8.3 The pull request's description, an index of evidence. Check:
      CI green

## 9. The close

- [ ] 9.1 `openspec archive parquet-fdw --yes`, the folder it makes
      under `openspec/changes/archive/` deleted, the contract moved
      beside the spec; the entry `parquet-fdw` leaves the roadmap, and
      what is left becomes entries. Check:
      `.github/scripts/check-specs.sh`
