## Why

TPC-H Q4 and Q21 test `EXISTS` and `NOT EXISTS` over `lineitem` by its
primary key, and the core's nested loop runs these semi and anti joins:
for each outer row an index scan of `lineitem_pkey`, which stops at the
first row that passes the inner clauses and the join's other clauses.

- Q4: 57 218 index scans, each returning 0.92 rows and removing about
  one by `l_commitdate < l_receiptdate` (EXPLAIN rounds the count
  removed); 57 ms of Q04 serial, 114 ms of processor time with two
  workers.
- Q21: an anti join of 156 739 index scans and a semi join of 13 859,
  each with a clause over both sides, `l3.l_suppkey <> l1.l_suppkey`;
  the anti join takes 84 ms of the query's 170.

TessHashJoin's lookup mode looks the keys of an outer batch up in one
scan of an index and builds the batch's table from the rows found. Its
semi and anti probe needs no change, but the planner offers the mode for
INNER and LEFT joins only, and the node refuses a lookup of any other
type when it starts. Its cost model would misjudge these joins as it
stands: it counts every row of a key for the nested loop, which stops at
the first match, and every candidate pair for the probe, which stops
there too. The lookup itself reads every row of a key. Whether one scan
a batch pays for those rows is what the change measures.

## What Changes

- The planner offers TessHashJoin's lookup mode for semi and anti
  joins, and the node accepts it. Execution stays: each outer batch's
  table holds the rows of its keys, and the semi and anti probe runs
  over it, a row without a key (NULL) answering as it does in the hash
  mode.
- The lookup's cost model keeps apart what it counts: the outer rows
  probed, the keys looked up, the rows the scan reads, the rows the
  table takes and the candidate pairs the probe checks. For a semi or
  anti join the nested loop reads a key's rows until the first match
  and the probe checks a row's candidates until the first match, by the
  core's own estimate of the matches; a batch of a semi or anti join
  goes out once, its rounds over a key's rows going on only while the
  join's other clauses reject them. INNER and LEFT lookups are priced
  as now.
- EXPLAIN's `Lookup Estimate` gains the nested loop's rows and the
  probe's checks.
- The calibration family `lookupcost` gains semi and anti samples,
  which check the model and are not fitted, and counts the rows the
  scan reads apart from the rows built. A sample's lookup is forced
  past the model's margin for its run, since the model may rightly
  refuse it.
- Before the measurement, at the maintainer's word: `bench/pg/run.sh`
  starts its cluster on the port its queries go to and checks the
  server it reaches; the families of the change `deform-past-varlena`
  are measured again with it.
- Tests: semi and anti joins by lookups against the core's rows, the
  lookup asserted in each plan, with NULL outer keys, keys repeated and
  absent, inner clauses that pass no row, some rows or every row of a
  key, a clause over both sides, a table past its memory, workers and a
  correlated rescan; the model's counts against the plan's.

Not in this change: a scan that stops at a key's first passing row.
Its index access needs PostgreSQL calls that the supported core no
longer offers (see the design); it becomes a roadmap entry, taken only
if the measurement shows Q4 needs it.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

None: results stay. The hash join has no capability of its own yet;
`docs/nodes.md` and `docs/costs.md` describe the lookup and its model
and take the change. Plans of semi and anti joins change where the
model now takes a lookup. The change declares `skip_specs: true`.

## Impact

- Code: `nodes/join_planner.c` (the join types, the model),
  `nodes/hashjoin_begin.c` (the join types at the start).
- Settings: none new.
- Tests: `test/sql/index_join.sql`.
- Benchmarks: `bench/pg/indexjoin.sql` (semi and anti cases),
  `bench/pg/lookupcost.sql` (their samples, the counts),
  `bench/pg/run.sh`.
- Documents: `docs/nodes.md`, `docs/costs.md`, the runs of
  `deform-past-varlena` in `docs/benchmarks/`, `openspec/roadmap.md`.
- The roadmap entry `semi-anti-lookups`.
