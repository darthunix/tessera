## Why

`TessAgg` groups an aggregate with DISTINCT of one argument, such as
`count(DISTINCT UserID)` by `RegionID`. It keeps a table of the pairs
of group and value it has seen, the DISTINCT set, and a row goes into
the aggregate only when it adds a new pair. Neither that set nor the
groups of such a query go to disk. So the planner gives the node such
a grouping only when two estimates fit `hash_mem`: the pairs, and,
beside a generic aggregate (one whose state the node keeps through
PostgreSQL's functions, such as `sum` of a `smallint`), the groups with
their states. Otherwise the core sorts every row and groups them in its
`GroupAggregate`.

Stage 2 of the change `parquet-fdw` measured ClickBench's queries over
its Parquet files, on an Apple M5 Pro, serial and at 8 workers. Two
queries are affected:

- Q9: `count(DISTINCT UserID)` by `RegionID`.
- Q10: the same grouping with `sum(AdvEngineID)`, `count(*)` and
  `avg(ResolutionWidth)` beside it.

What it found:

- Over ten of ClickBench's files, 10 M rows, the node takes Q9. It ran
  serially in 0.29 s against the core's 2.25 s.
- Over `hits.parquet`, 100 M rows, the pairs of region and user do not
  fit `hash_mem` by the estimate. Q9 and Q10 go to the core: 469 and
  532 billion instructions serially, 20.3 and 22.6 s, against DuckDB's
  1.7 and 2.5 s. At 8 workers they take 9.2 and 10.4 s.
- Q10 goes to the core over the ten files too, where its pairs fit. Its
  `sum` and `avg` of `smallint` are generic aggregates. The gate of
  their states takes the count of groups from the core's hashed
  grouping paths, and the core builds no hashed path when an aggregate
  has DISTINCT. So the gate never passes. The design of the node says
  the gate uses the planner's estimate of the groups: the code and the
  document disagree.
- At 8 workers over the ten files Q10 took 2.03 s with Tessera on and
  1.17 s off. It is the one query of the stage where Tessera on lost to
  Tessera off, against the stage's criterion.

The maintainer asked on 2026-11-22 to fix both causes, in a change of
its own off `main`.

## What Changes

- The gate of a grouping with a DISTINCT aggregate weighs its groups
  and their states by the planner's estimate of the groups before
  HAVING, which the core's grouped paths carry, as the design of the
  node says. Q10 over the ten files goes to the node.
- The DISTINCT set of a grouping spills. Past seven eighths of
  `hash_mem` it takes no new pair, and a row whose pair it lacks goes to
  disk, by the bits of the pair's hash. Once the input is read, each
  partition is read back into an empty set, and the rows that add a new
  pair go into their group's aggregate, which is still in memory. A
  partition that does not fit splits into a level below. Each pair is
  counted once: in memory or in the one partition its hash names. The
  machine that decides the freeze, the levels and the order of reading
  is the one that the rows of generic aggregates already use.
- The groups of such a query still do not spill. The planner gives it
  to the node when its groups and their states fit `hash_mem` and its
  pairs fit too, as today, or, now, when its pairs do not fit but its
  groups take at most half of `hash_mem`, leaving the rest to the sets.
  There alone a set weighs the groups by their estimate too, to keep
  room for the groups still to come. Where the pairs fit by the
  estimate, a set freezes by the memory the node holds, so a grouping
  whose groups the planner thinks more than they are runs as today.
  The pairs' spill is costed as the rows' spill is.
- EXPLAIN (ANALYZE) counts the partitions and the disk of the pairs in
  `Batches` and `Disk Usage`; VERBOSE adds `Spilled Pairs`.
- The family `spill` of `bench/pg` gets a grouped `count(DISTINCT)`
  whose pairs pass `hash_mem`.

Not in this change:

- The set of an argument a word does not hold (`text`, `numeric`). Its
  values are numbered in a dictionary of the set's own, which grows with
  every new value, also of a pair that went to disk. Such a set keeps
  its gate of the pairs.
- A DISTINCT aggregate without `GROUP BY`, one set with no groups.
  It keeps its gate.
- The groups of a grouping with DISTINCT going to disk, and a parallel
  form of such a grouping. Q9 and Q10 at 8 workers stay serial in the
  node.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

- `table-memory`: a DISTINCT set of a grouping goes to disk past
  `hash_mem`, and what its spill counts and holds; which groupings
  with a DISTINCT aggregate the node takes by their memory.

## Impact

- `nodes/agg_planner.c`: the gates and the cost of a grouping with
  DISTINCT.
- `nodes/agg_keydict.c`, `nodes/agg_group.c`, `nodes/agg_spill.c`,
  `nodes/agg.c`, `nodes/agg_node.h`, `nodes/agg.h`: the set's freeze,
  its writers and its reading back; EXPLAIN.
- `crates/relspill`: no change of the API. A property test of a set's
  pairs through frozen sets and their levels, on its `Rows` machine.
- SQL suites: `test/sql/agg.sql` and its expected files.
- `bench/pg/spill.sql`; docs: `docs/nodes.md`, `docs/limitations.md`,
  `docs/costs.md`, `openspec/specs/table-memory/design.md`.
- No public header, no setting, no format on disk changes.
- The roadmap's entry "A grouped `COUNT(DISTINCT)` left to the core",
  which the change `parquet-fdw` adds to "Not placed", is what this
  change takes up. It is not on `main` yet: if `parquet-fdw` merges
  first, the close of this change removes it; if this change closes
  first, `parquet-fdw` drops it when it is rebased. Its other entry, on
  the serial plans chosen over the core's parallel ones, stays.
- A scenario of table-memory measures a DISTINCT set at `work_mem` =
  256 kB, where the set would now spill; it moves to 384 kB, where its
  later chunks still take the least length of 64 kB.
