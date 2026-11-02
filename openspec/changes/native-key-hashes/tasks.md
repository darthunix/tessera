## 1. The proposal

- [x] 1.1 The proposal, the design and the tasks, with `skip_specs`,
      and the roadmap: this change, the entry for numeric keys and the
      findings of the profiles. Check:
      `openspec validate native-key-hashes --strict`;
      `.github/scripts/check-specs.sh`

## 2. The work

- [ ] 2.1 `tess_key_bytes`, and the dictionary by it, char(n) included;
      tests in `types.sql`. Check: `make installcheck` on PostgreSQL
      master with assertions; the tests fail without the trim
- [ ] 2.2 The join's hash by the bytes; tests in `join.sql`. Check: the
      same
- [ ] 2.3 The measurement of the design, its runs in the design

## 3. The close

- [ ] 3.1 Close the change: its folder goes, the runs to
      `docs/benchmarks/`, findings to "Not placed". Check:
      `.github/scripts/check-specs.sh`
