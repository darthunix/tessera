## 1. The proposal

- [x] 1.1 The proposal, the design and the tasks, with `skip_specs`,
      and the roadmap entries of the TPC-H analysis. Check:
      `openspec validate deform-without-restarts --strict`;
      `.github/scripts/check-specs.sh`

## 2. The work

- [ ] 2.1 The second cursor a row, and its test. Check: `make
      installcheck` on PostgreSQL master with assertions; the test fails
      without the second cursor
- [ ] 2.2 The earlier of two compared columns first, and its test.
      Check: the same; the test shows restarts without it
- [ ] 2.3 The measurement of the design

## 3. The close

- [ ] 3.1 Close the change: the folder goes, the runs to
      `docs/benchmarks/`, the entry leaves the roadmap. Check:
      `.github/scripts/check-specs.sh`
