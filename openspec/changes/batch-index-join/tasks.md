## 1. The proposal

- [x] 1.1 The proposal, the design, the tasks and the changes of the
      specs `table-memory` and `key-filter`. Check: `openspec validate
      batch-index-join --strict`; `.github/scripts/check-specs.sh`

## 2. The work

- [ ] 2.1 The benchmark family of index nested loops, measured on the
      base
- [ ] 2.2 The lookup mode of TessHeapScan (its plan, start, call,
      rescan, end and EXPLAIN, every mode switch of the node) with the
      join's table per outer batch, INNER and LEFT, what assumes one
      table switched off, the reuse of a batch's table: one commit,
      since no SQL reaches either without the other
- [ ] 2.3 The planner: the lookup path inside the join's path, its
      eligibility through the scan planner's helpers, the admission by
      statistics, the cost in the nested loop's currency, serial and
      partial, its prices settings fitted by the family `lookupcost`,
      and `docs/costs.md`
- [ ] 2.4 The tests, each with its plan and against the core: batches
      with pairs, without and with NULL keys only, one after another;
      duplicates on both sides; sparse batches; other equalities; LEFT
      rejected by `ON` and by `WHERE`; inner clauses and computed
      columns; row output, a cursor read in part, a limit, rescans with
      no and with an outer parameter changed; a HOT update; a prepared
      plan after its index is dropped and made again; refused indexes
      (partial, expression, other family), an inheritance parent, RLS;
      text, date, numeric and char(n) keys, integer keys of two types; a
      nondeterministic collation refused; a skewed key past `hash_mem`
      with its `Overrun`, at a changed `work_mem` too; a Gather with and
      without the leader, its rescan; the specs' scenarios
- [ ] 2.5 The measurement of the design, for the maintainer

## 3. The close

- [ ] 3.1 Close the change: the requirements to the specs, the runs to
      `docs/benchmarks/`, the entry leaves the roadmap, SEMI and ANTI
      and the split of a batch's keys become entries. Check:
      `.github/scripts/check-specs.sh`
