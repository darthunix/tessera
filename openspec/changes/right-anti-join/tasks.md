## 1. The proposal

- [x] 1.1 The proposal, the design, the tasks and the changes of the
      specs `key-filter` and `partition-pruning`.
      Check: `openspec validate right-anti-join --strict`;
      `.github/scripts/check-specs.sh`

## 2. The work

- [ ] 2.1 TessHashJoin takes `JOIN_RIGHT_ANTI`: the planner's paths and
      cost, the node's start, the probe without pairs, EXPLAIN; the
      tests of the join and parallel suites. Check: `make installcheck`
      on PostgreSQL master with assertions
- [ ] 2.2 The benchmark case and the measurement of the design

## 3. The close

- [ ] 3.1 Close the change: the requirements to the specs, the
      designs of `key-filter` and `partition-pruning` name RIGHT ANTI,
      the folder goes, the runs to `docs/benchmarks/`, the entry leaves
      the roadmap. Check:
      `.github/scripts/check-specs.sh`
