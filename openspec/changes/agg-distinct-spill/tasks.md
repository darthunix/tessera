## 0. How the work goes

Every task is one commit on the branch `agg-distinct-spill`, but the
opening of a pull request, which is a step of the work. Each commit
passes `make rust-check` and `.github/scripts/check-specs.sh`, run with
the version of OpenSpec that `ci.yml` names; the check named in a task
comes on top. A task that changes C runs `make installcheck` on
PostgreSQL master with assertions and injection points, and on 19 and
18. A run that loads the machine waits for the maintainer's word, as
`docs/measuring.md` says, and takes five minutes at most. The work
starts when the maintainer has approved this proposal. The proposal is
one pull request; the work and the close are a second.

## 1. The proposal

- [x] 1.1 Write the proposal, the design, the spec's delta and the
      tasks of this change, and its entry in the roadmap, second in the
      queue. Check: `openspec validate agg-distinct-spill --strict`;
      `.github/scripts/check-specs.sh`

## 2. The gate of the groups

- [x] 2.1 Every grouping with a DISTINCT aggregate asks that its groups
      with their states fit `hash_mem`, its groups the most `numGroups`
      of the core's grouped paths in the relation, in place of its
      hashed paths alone; the gate of the pairs stays as it is. Tests in
      `test/sql/agg.sql`: the plans of the scenario "A generic aggregate
      beside DISTINCT" and of a grouping whose groups pass `hash_mem` by
      the estimate, their rows against the core's. `docs/nodes.md` says
      which estimate. Check: `make installcheck`

## 3. The pairs through the machine

- [x] 3.1 A property test of `relspill`: random pairs of group and
      value through a set frozen by `Rows` under a random limit, its
      pairs not found written by their hash, its levels read back into
      sets of their own and frozen again, count the distinct pairs of
      each group as a set in memory does. No production code. Check:
      `cargo test --locked -p relspill`
- [ ] 3.2 The family `spill` of `bench/pg` gets the cases of the
      design's measurement plan, named in its lists and in
      `bench/pg/README.md`. Check: the family at `repetitions` 1 on the
      maintainer's word, its plans showing the core's grouping at 4 and
      16 MB, where the pairs do not fit by the estimate, and `TessAgg`
      at 64 MB

## 4. The set that spills

- [ ] 4.1 The set of a grouping is keyed by its group's reference and
      the argument; nothing else changes. Tests: the suites as they
      are, and DISTINCT cases of `test/sql/agg.sql` with keys of
      several kinds and several keys. `docs/nodes.md` says the set's
      key. Check: `make installcheck`; the counts of the measurement
      plan's item 2 at 64 MB against `main`, on the maintainer's word
- [x] 4.2 The set of an argument a word holds freezes on its machine's
      word, writes the pairs it lacks, and reads its levels back into
      their groups before the groups go out; every set is emptied at the
      end of the input; Memory Usage counts the writers and readers and
      the groups by their bytes, and so does the machine of each set
      until task 4.4 passes it the estimate; the sets are read back as
      of the last level when the groups' bytes alone pass seven eighths
      of the limit; a rescan and the node's end close the levels. The
      case of the scenario "A DISTINCT set under a small limit" moves to
      `work_mem` = 384 kB, its 245 kB confirmed, and the cases of two
      sets to 384 kB, or to 512 kB if they pass seven eighths there.
      Tests in `test/sql/agg.sql`, over a table whose statistics hold
      fewer pairs than it has (`autovacuum_enabled = off`), so that the
      planner gives the node a set past the limit: the scenarios "Pairs
      past the limit" but its EXPLAIN, "Aggregates through the spill", a
      rescan with levels unread, a cursor's partial fetch, groups that
      come after the sets froze, a reading back whose groups alone pass
      seven eighths of the limit and one whose groups do not, against
      the core. The design of the node in `docs/nodes.md` and
      `openspec/specs/table-memory/design.md` updated. The commit says
      why it is not split if it passes 500 lines. Check:
      `make installcheck`
- [x] 4.3 EXPLAIN counts the pairs' partitions and disk in `Batches`
      and `Disk Usage`, and VERBOSE shows `Spilled Pairs`. Tests: the
      EXPLAIN of the scenarios "Pairs past the limit" and "A partition
      read back past the limit". Check: `make installcheck`
- [ ] 4.4 The planner offers the node a grouping whose pairs do not fit
      by the estimate, with `GROUP BY`, word arguments and groups within
      half of `hash_mem`, costs the pairs' spill, and on that way alone
      passes the node the estimate of the groups' bytes, which the
      machines of its sets weigh while the input is read. Tests: the
      plans of the scenarios "Pairs past the limit by the estimate",
      "Groups past half the limit by the estimate" and "Text arguments
      past the limit by the estimate"; the scenario "Groups thought more
      than they are", against the core. `docs/costs.md` and
      `docs/limitations.md` updated.
      Check: `make installcheck`

## 5. The measurement

- [ ] 5.1 The runs of the design's measurement plan, on the
      maintainer's word: the family `spill`, the counts of the path
      that fits against `main`, and ClickBench Q9 and Q10 on a local
      merge with the branch of `parquet-fdw`. The runs kept in
      `docs/benchmarks/`, as `docs/measuring.md` says. Check: the
      thresholds of the plan, or the finding that stops the work

## 6. The close

- [ ] 6.1 Archive the change: the delta into
      `openspec/specs/table-memory/spec.md` with its tests named, the
      design's decisions into the design of table-memory and the
      node's text, the roadmap's entry of this change out, and, if
      `parquet-fdw` merged first, its entry of the grouped
      `COUNT(DISTINCT)` too; what is left as new entries. Check:
      `openspec validate --specs --strict`;
      `.github/scripts/check-specs.sh`
