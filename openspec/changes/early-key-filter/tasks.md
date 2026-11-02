## 1. The proposal

- [x] 1.1 The proposal, the design, the tasks and the change of the spec
      `key-filter`. Check: `openspec validate early-key-filter
      --strict`; `.github/scripts/check-specs.sh`

## 2. The work

- [x] 2.1 TessFilter of batch clauses alone takes the filter and checks
      it first; the tests. Check: `make installcheck` on PostgreSQL
      master with assertions
- [x] 2.2 A small table's filter, to hand down only, serial and shared;
      the tests
- [ ] 2.3 The benchmark case of the worst order and the measurement of
      the design

## 3. The close

- [ ] 3.1 Close the change: the requirements to the spec, the design of
      `key-filter`, the runs to `docs/benchmarks/`, the entry leaves the
      roadmap. Check: `.github/scripts/check-specs.sh`
