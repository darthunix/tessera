## 0. How the work goes

The branch `parquet-batch-arrays` starts from `main` at the close of
`agg-date-min-max`. The maintainer approves the proposal before any
code. The work goes in one pull request into `main`, reviewed by Codex
and Claude Fable. Every task is one commit, except 4.2, the pull
request's description. Each commit passes
`.github/scripts/check-specs.sh` and the check named in its task. "The
suites" means `make && make install` and `make installcheck
EXTRA_REGRESS_OPTS="--temp-instance=$PWD/test/tmp_check"` on PostgreSQL
master built as CI builds it, with assertions and injection points; CI
runs 19 and 18.

## 1. The proposal

- [x] 1.1 Write the proposal, the design and the tasks of this change.
      Check: `openspec validate parquet-batch-arrays --strict`;
      `.github/scripts/check-specs.sh`

## 2. The arrays kept for the scan

- [x] 2.1 `parquet/node.c`: `node_begin` allocates the batch's arrays
      for the plan's columns; `batch_column` and `batch_bytes` index
      them by the scan's column and allocate a column's arrays of 64
      rows, zeroed, at its first ask in the scan, in the query's
      memory; `next_batch` and a rescan clear the rows made of the
      scan's columns in both forms and the views' dictionaries, and
      wipe the values and the bytes' places under assertions;
      `next_batch` asserts that a batch holds at most 64 rows and
      allocates nothing; `make_bytes` writes zeros for an asked `NULL`
      row. The comments that say where a batch's values live follow,
      `ParquetBatch`'s and `ParquetNode`'s `batch_cxt`.
      `test/tessera_parquet_test.c`: `parquet_test_asks` and
      `parquet_test_bytes` take an optional last argument to ask every
      target's column of each batch they skip, as `Datum` values and in
      the bytes form, the second again after its rescan; the second
      reports an ask of no row and fails on an asked `NULL` row with a
      place, a length or an index; their comments say so.
      `test/sql/parquet.sql` and `test/sql/parquet_cancel.sql`: the
      calls with the argument true beside the calls as they are, the
      ask of no row, and the table whose first batch is short, as the
      design's "Tests" lists them; the expected output gains their rows
      alone. Check: the suites; `make crosscheck`

## 3. The count

- [ ] 3.1 Count as the design's measurement plan says, at the
      maintainer's word that the machine is idle: a worktree of the
      base beside the branch's, release builds checked, plans compared,
      the base and the end in turns, twice, with the control and
      pg_duckdb's cycles beside, and profiles of Q1 and Q3 at both.
      Keep the report under `docs/benchmarks/`. Check:
      `.github/scripts/check-specs.sh`

## 4. The close

- [ ] 4.1 `openspec/specs/parquet-fdw/design.md` says that a batch's
      arrays are the scan's, for its columns, zeroed once, wiped under
      assertions, and their rows made cleared at each batch, why, and
      that the memory reset at each batch holds the arenas of strings,
      the references to windows and the checks' scratch; the forecast
      of `contract.md` gains "how the node keeps a batch's values", the
      node alone; the roadmap loses `parquet-batch-arrays`. The
      change's folder is deleted. Check: `openspec validate --all
      --strict`; `.github/scripts/check-specs.sh`
- [ ] 4.2 The pull request's description, as CONTRIBUTING.md asks of a
      pull request that closes a change; under Safety, CI's job of the
      sanitizers for the arrays' lifetime.
