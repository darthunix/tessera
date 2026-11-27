## 0. How the work goes

The branch `parquet-integer-columns` starts from `main` at the close of
`parquet-batch-arrays`. The maintainer approves the proposal before any
code. The work goes in one pull request into `main`, with the
implementation, reviewed by Codex and Claude Fable. Every task is one
commit, except 4.2, the pull request's description. Each commit passes
`.github/scripts/check-specs.sh` and the check named in its task. "The
suites" means `make && make install` and `make installcheck
EXTRA_REGRESS_OPTS="--temp-instance=$PWD/test/tmp_check"` on PostgreSQL
master built as CI builds it, with assertions and injection points; CI
runs 19 and 18.

## 1. The proposal

- [x] 1.1 Write the proposal, the design and the tasks of this change.
      Check: `openspec validate parquet-integer-columns --strict`;
      `.github/scripts/check-specs.sh`

## 2. The rows not asked left alone

- [x] 2.1 `crates/datum-arrow/src/read.rs`: `dense` and `dense_bits`
      convert the words of the mask from the first that asks a slot to
      the last, the others' places kept and their words of `NULL`
      slots written 0; their comments say so.
      `crates/datum-arrow/src/read/tests.rs`: the tests of the
      design's "Tests" for `datum-arrow`. Check: `make rust-check`;
      the commands of CI's job "datum-arrow alone"; the machine code of
      the dense loops at the base and here, by `cargo rustc --release
      -p datum-arrow --lib -- --emit asm`; `cargo mutants --in-diff`
      on the diff; the suites; `make crosscheck`
- [x] 2.2 `crates/tessera-parquet/src/c.rs`: the reader's handle keeps
      the mask of `NULL` rows that `tess_parquet_values` gives the read,
      sized to the caller's mask at each call;
      `crates/tessera-parquet/src/c/tests.rs`: two windows of different
      sizes on one reader, and a caller's mask one word short refused.
      Check: `make rust-check`; Miri of the crate's C entry points as
      CI runs it; `cargo mutants --in-diff` on the diff; the suites

## 3. The count

- [x] 3.1 Count as the design's measurement plan says, at the
      maintainer's word that the machine is idle: a worktree of the
      base beside the branch's, release builds checked, plans compared,
      the base and the end in turns, twice, with the control and
      pg_duckdb's cycles beside, profiles of Q3 at both, and the times
      of Q3, Q4 and Q7 after the counts. Keep the report under
      `docs/benchmarks/`. Check:
      `.github/scripts/check-specs.sh`

## 4. The close

- [ ] 4.1 `openspec/specs/datum-arrow/design.md` says that a dense loop
      converts the words from the first asked to the last, and why;
      the forecasts gain "how a dense loop goes over a mask" in
      `datum-arrow`'s contract and "the reader's work for a call of a
      window's values" in `parquet-fdw`'s; what the count found of the
      node's copy and of a form of integers in their own width goes to
      the roadmap, as an entry or into "Not placed"; the roadmap loses
      `parquet-integer-columns`. The change's folder is deleted. Check:
      `openspec validate --all --strict`;
      `.github/scripts/check-specs.sh`
- [ ] 4.2 The pull request's description, as CONTRIBUTING.md asks of a
      pull request that closes a change.
