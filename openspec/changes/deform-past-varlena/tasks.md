## 1. The proposal

- [x] 1.1 The proposal, the design and the tasks, with `skip_specs`.
      Check: `openspec validate deform-past-varlena --strict`;
      `.github/scripts/check-specs.sh`

## 2. The work

- [x] 2.1 The walk that keeps columns, rows with NULLs and short rows
      too, the counts and the window that choose them, and their
      tests. Check: `make installcheck` on PostgreSQL master with
      assertions; the tests show restarts without it
- [x] 2.2 The share that keeps a column, set by the measurement of
      Q01, Q06, Q12 and Q14
- [x] 2.3 The path for a one-byte header, measured on Q06 and Q14;
      kept only with a gain
- [ ] 2.4 The measurement of the design

## 3. The close

- [ ] 3.1 Close the change: the folder goes, the runs to
      `docs/benchmarks/`, the entry leaves the roadmap,
      `docs/runtime.md` takes the design. Check:
      `.github/scripts/check-specs.sh`
