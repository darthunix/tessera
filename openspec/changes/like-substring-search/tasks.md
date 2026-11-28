## 1. The proposal

- [x] 1.1 The proposal, the design and the tasks, approved by the
      maintainer. Check: `openspec validate like-substring-search
      --strict`; `.github/scripts/check-specs.sh`

## 2. The family and the tests

- [x] 2.1 `bench/pg/like.sql`, the family of the design's measurement
      plan, over a table of its own, each case's plan read in
      `plans.txt`; `bench/pg/README.md` describes it. No code changes:
      this commit is the base of the count. Check: the family run once
      with `cargo ab measure --base HEAD`, every case through TessFilter
      or the kernels' function as the plan says
- [x] 2.2 The property test of the matcher against its reference over
      strings of 0 to 300 bytes and pieces of 1 to 40 bytes at every
      place, so that a search over vectors runs. Check: `make
      rust-check`

## 3. The search

- [x] 3.1 The three ways of the design, each on a branch of its own
      that is never merged, counted on the family's cases of inner
      pieces and its guards by `cargo ab measure --counters --cycles
      --rounds 2`, with the machine code of each loop by `tessera-bench
      --disasm`; the counts kept in the report's folder. Check: the
      counts read, the way with the fewest cycles that keeps the guards
      named in the design
- [x] 3.2 The way kept, in `Like`, with its tests, and the design's
      "Which searcher" updated with the counts of the others. Check:
      `make rust-check`; the suites; mutants of the changed lines

## 4. The count

- [ ] 4.1 Count as the design's measurement plan says: the family by
      cycles and instructions, ClickBench's Q21 and Q22 with the
      control, TPC-H's query 13 by time, against 2.1. Keep the report
      under `docs/benchmarks/`. Check: `.github/scripts/check-specs.sh`

## 5. The close

- [ ] 5.1 What the design decided goes where it outlives the change:
      `docs/kernels.md` describes the search of an inner piece and
      why it was chosen; the roadmap loses the entry
      `like-substring-search`, and what is left becomes entries. The
      change's folder is deleted. Check: `openspec validate --specs
      --strict`; `.github/scripts/check-specs.sh`
- [ ] 5.2 The pull request's description, as CONTRIBUTING.md asks of a
      pull request that closes a change.
