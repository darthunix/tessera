## 1. The proposal

- [x] 1.1 The proposal, the design and the tasks, with `skip_specs`.
      Check: `openspec validate bpchar-constant-compare --strict`;
      `.github/scripts/check-specs.sh`

## 2. The work

- [x] 2.1 `Strings::constant` and the prepared scalar in `compare`, the
      tests and the mutants' exclusion. Check: `make rust-check`; `make
      installcheck` on PostgreSQL master with assertions
- [ ] 2.2 The measurement of the design

## 3. The close

- [ ] 3.1 Close the change: the folder goes, the runs to
      `docs/benchmarks/`, the entry leaves the roadmap. Check:
      `.github/scripts/check-specs.sh`
