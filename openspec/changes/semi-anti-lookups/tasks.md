## 1. The proposal

- [x] 1.1 The proposal, the design and the tasks, with `skip_specs`.
      Check: `openspec validate semi-anti-lookups --strict`;
      `.github/scripts/check-specs.sh`

## 2. The work

- [x] 2.1 `bench/pg/run.sh`: every start names the port, the server's
      data directory checked before a family on both of `measure`'s
      paths. Check: `bench/pg/run.sh measure tax` after a start on
      another port; `cargo ab measure` (the path of
      `BENCH_RESTARTED`); `BENCH_RESTARTED=1` with `PGPORT` naming
      another cluster, which must stop
- [ ] 2.2 The families `tax` and `win` of `deform-past-varlena` again,
      their runs in its benchmarks. Check: the runs
- [x] 2.3 The model's counts kept apart, the semi and anti counts from
      the core's factors, `Lookup Estimate`'s new fields; INNER and
      LEFT priced as before. Check: `make installcheck`; the plans of
      `test/sql/index_join.sql` unchanged but for the estimate
- [x] 2.4 Lookups for semi and anti joins in the planner and in
      `join_begin`, and their tests. Check: `make installcheck` on
      PostgreSQL master with assertions; `make crosscheck`
- [x] 2.5 `lookupcost`: the rows read apart from the rows built; the
      semi and anti samples, one of two keys, each lookup forced past
      the margin, the loop's rows by a query in the index's order, the
      probe's checks by `Matches`; `indexjoin`'s semi and anti cases.
      Check: `lookupcost` run, its summary
- [ ] 2.6 The measurement plan of the design, against its thresholds

## 3. The close

- [ ] 3.1 Close the change: the folder goes, the runs to
      `docs/benchmarks/`, the entry leaves the roadmap and an entry for
      the first-row scan takes its findings, `docs/nodes.md` and
      `docs/costs.md` take the design. Check:
      `.github/scripts/check-specs.sh`
