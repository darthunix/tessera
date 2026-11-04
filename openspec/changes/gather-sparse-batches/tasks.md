## 1. The proposal

- [x] 1.1 The proposal, the design and the tasks, with `skip_specs`.
      Check: `openspec validate gather-sparse-batches --strict`;
      `.github/scripts/check-specs.sh`

## 2. The work

- [ ] 2.1 The heap batch's reading of decimals moved to a runtime
      function, a commit of its own. Check: `make installcheck`
- [ ] 2.2 `values_stored` in `TessBatch`, set by each producer on each
      batch it publishes, by the design's rules for every kind of
      output, and its tests. Check: `make installcheck` on PostgreSQL
      master with assertions and injection points
- [ ] 2.3 The gathering in `TessInput`, `tess_input_set_gather`,
      `tess_input_gathered_rows`, the gathered batch and its C test.
      Check: `make installcheck`
- [ ] 2.4 TessHashJoin and TessAgg gather; the join's tuple bound,
      scan by scan; `Gathered Rows`; the SQL suites' cases; the
      reading ahead and the volatile functions in
      `docs/limitations.md`. Check: `make installcheck`; `make
      crosscheck`
- [ ] 2.5 The lookup's count of gathered batches in every branch of
      its shape, the span in a participant's units, its two descent
      counts apart, and the copy's price, fitted by `lookupcost`;
      `Lookup Estimate` beside the batches received in `index_join`.
      Check: the calibration's runs; `docs/costs.md`
- [ ] 2.6 The measurement plan of the design, against its thresholds

## 3. The close

- [ ] 3.1 Close the change: the folder goes, the runs to
      `docs/benchmarks/`, the entry leaves the roadmap,
      `docs/runtime.md`, `docs/costs.md` and `docs/nodes.md` take the
      design. Check: `.github/scripts/check-specs.sh`
