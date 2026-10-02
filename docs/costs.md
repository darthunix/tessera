# Planning costs and their calibration

PostgreSQL's planner keeps the cheapest path of every relation, so a
Tessera node runs only where its path costs less than the core's. This
document says how the module prices its paths, what each parameter
stands for, how the parameters were measured, and where the comparison
with the core's own costs is only as good as the core's. The code is in
`nodes/scan_planner.c` (the scans and the filter), `nodes/join_planner.c`
(`join_cost`), `nodes/agg_planner.c` (`group_cost`, `plain_cost`) and
`nodes/module.c` (the parameters); the calibration families are
`bench/pg/scancost.sql` and `bench/pg/joincost.sql` (see
[bench/pg/README.md](../bench/pg/README.md)).

## Principles

- **Children carry their own costs.** A node's path costs its children as
  they are and adds the node's own work. Nothing takes a share of the
  whole path: such a share, applied to a child that was already a node of
  the module, compounded over a tree of joins, and the planner chose a
  join order by the count of the module's nodes rather than by their time
  (plan 8.10: Q5 of TPC-H read every row of `lineitem` and ran 1.4 times
  the core's time).
- **Own work from measured time.** Where a node does its work in another
  way than the core, in batches through the kernels, its time is a model
  of its own: a price a page, a row, a pair, a batch, fitted by least
  squares over a family of measured queries. Where a node does the same
  work the core does, row by row through the core's functions, it costs
  a share of what the core counts for it, the share measured once.
- **One unit.** The models' prices are in units of the scan model: a unit
  is the node's time to read one page of a full scan
  (`tessera.scan_page_cost` = 1), 0.27 µs on the reference machine. The
  core prices a page of a sequential scan at `seq_page_cost` = 1 as
  well, so a unit of the models is a unit of the core's cost for what the
  core and the node both read, and the scans are priced against the
  core's own scans of the relation (below).
- **An exchange rate where the core's costs are nominal.** The core's
  constants are not times: over the same joins the core's hash join takes
  4.9 µs of real time per unit of the cost the core gives it, where a unit
  of the scan model is 0.27 µs. A node that competes with such a core node
  converts its time at a measured rate, so that the planner's choice
  between the two follows their times.
- **The rows are the core's.** The models take the rows the planner
  expects from the core's estimates; a wrong estimate misleads the node's
  path and the core's alike, and the module does not correct it.

## The reference machine

The defaults are the values measured on the project's machine, an Apple
M5 Pro laptop on mains power, PostgreSQL master, the data in shared
buffers, as of 2026-10-02. Another machine, or data on disk, calls for
other values, as the core's own `random_page_cost` does: run the
calibration families there (below) and set the parameters they print.
Every parameter is a session setting any user may set, and
`EXPLAIN (SETTINGS)` lists those not at their defaults.

## Scans and the filter

`TessHeapScan` reads a relation in batches, serially or as a participant
of a parallel scan, in four modes: the full scan, and over the core's
index, index-only and bitmap scans; `TessFilter` above it applies the
relation's clauses, in batches where the expression compiler takes them
and row by row otherwise (see [nodes.md](nodes.md), TessHeapScan and
TessFilter). The model gives each of the node's scans a time:

- a full scan: `scan_page_cost` a page and `scan_tuple_cost` a row of the
  table;
- an index-only scan: `index_only_tuple_cost` a row the index's
  conditions select (a row on a page not all visible as an index scan's
  row);
- an index scan: `index_tuple_cost` a row selected;
- a bitmap: `bitmap_page_cost` a page the bitmap reads and
  `bitmap_tuple_cost` a row, and `bitmap_scatter_cost` more a row times
  1 - c² for the correlation c of the index's first column, since a
  bitmap of a column out of the table's order is built from rows in no
  order of their pages; the pages are the core's estimate for rows at
  random places, moved toward the pages the rows fill in the table's
  order by c²;
- the filter's work past the first clause, over the rows entering it (all
  the table's for a full scan, those the index gives otherwise):
  `filter_clause_cost` a row of each later batch clause, over the rows the
  earlier clauses left; `filter_row_clause_cost` a row of a clause
  evaluated row by row and `filter_row_operator_cost` a row of each of
  its operators by the core's cost of it; `deform_varlena_cost` a row of a
  clause's column past a column of varying length, which must be deformed;
- a partial scan: the workers' start and finish,
  `scan_parallel_setup_cost`, then the serial time shared among the
  participants, a worker reading a page in the serial time plus
  `scan_worker_page_cost`, the toll of mapping a page of the shared
  buffers it reads first (near zero with huge pages); a partial index or
  index-only scan counts a worker at `index_worker_share` of the leader's
  pace, since the core's parallel btree scan hands its leaf pages on one
  at a time, and a partial bitmap one participant's building of the
  bitmap, `bitmap_build_cost` a row and `bitmap_build_scatter_cost` more
  times 1 - c².

The times become costs by ranking: after the node's paths of a relation
are added, each at `scan_cost_factor` of the core's path it replaces, the
hook ranks the paths of the relation's serial list, the core's and the
node's, by the model's times (a core scan timed as the node's scan of the
same kind, a floor of the core's own time), and each of the node's paths
costs just below (0.99) the cheapest of the paths the model finds slower,
never lower, so that the relation's cheapest cost, which the joins above
read, hardly moves. The serial ranking leaves the fastest serial scan's
cost per unit of its time, and the node's partial paths cost their time
at that price, less `parallel_setup_cost`, which the gather above adds
back. The core's own costs do not take part: the core's time per unit of
its cost varied four times over its bitmaps.

| Parameter | Default | The node's time for |
|---|---|---|
| `tessera.scan_cost_factor` | 0.9 | the core scan's cost a node's path starts at, before the ranking |
| `tessera.scan_page_cost` | 1 | a page of a full scan, the unit |
| `tessera.scan_tuple_cost` | 0.0077 | a row of a full scan with its filter |
| `tessera.index_only_tuple_cost` | 0.058 | a row of an index-only scan |
| `tessera.index_tuple_cost` | 0.112 | a row of an index scan |
| `tessera.bitmap_page_cost` | 0.665 | a page of a bitmap |
| `tessera.bitmap_tuple_cost` | 0.075 | a row of a bitmap |
| `tessera.bitmap_scatter_cost` | 0.031 | what a row of a bitmap takes more, times 1 - c² for the correlation c of the index's first column |
| `tessera.filter_clause_cost` | 0.0053 | a row of a batch clause past the filter's first |
| `tessera.filter_row_clause_cost` | 0.022 | a row of a clause the filter evaluates row by row |
| `tessera.filter_row_operator_cost` | 0.018 | a row of an operator of such a clause, by the core's cost of it |
| `tessera.deform_varlena_cost` | 0.017 | a row of a clause's column past one of varying length, deformed |
| `tessera.scan_parallel_setup_cost` | 7000 | the start and finish of a partial scan's workers, 1.85 ms here |
| `tessera.scan_worker_page_cost` | 3.15 | what a worker takes more for a page of the shared buffers it reads first, 0.83 µs here |
| `tessera.bitmap_build_cost` | 0.038 | a row of a partial bitmap's building, which one participant does |
| `tessera.bitmap_build_scatter_cost` | 0.054 | what building takes more a row, times 1 - c² |
| `tessera.index_worker_share` | 0.5 | the share of the leader's pace a worker of a partial index or index-only scan reads at (a share, not a time) |

Measured by `bench/pg/scancost` (pg-scancost-PdAvZF, JFWpYj, oxfz57,
paePJX): the full scan 0.26 µs a page and 2.0 ns a row (its six tables
of 30 to 140 rows a page within 12 %), the index-only scan 15 ns a row,
the index scan 29 (within 4 %), the bitmap 0.17 µs a page and 19 ns a
row, 8 more for a scattered column (within 5 % but at 1 % of the rows);
a later batch clause 1.4 ns a row, a clause by rows 5.8 and 4.8 an
operator, a column past a varlena 4.5 (eleven samples within 0.6 ms);
the workers' start 1.85 ms and the toll 0.83 µs a page, the index
worker's share 0.50 (ten samples within 4 %). The gates that keep a mode
out regardless of cost, `tessera.bitmap_page_rows`,
`index_min_correlation` and `index_min_rows`, are in
[nodes.md](nodes.md), Parameters.

## The hash join

`TessHashJoin` builds the inner side into the kernels' hash table and
probes it with the batches of the outer side (see [nodes.md](nodes.md),
TessHashJoin). Its path costs its children as they are and the node's
own time, from the rows the planner expects of each side (B built, P
probed) and of the join (M):

- building: `join_build_cost` a row of B, `join_text_value_cost` more
  for every by-reference inner value kept (text, numeric, ...) and
  `join_hashed_key_cost` more for every key a word does not hold, hashed
  by its type's function;
- probing: `join_probe_cost` a row of P, and `join_hashed_key_cost` more
  a hashed key; where the planner expects the Bloom filter (fewer probe
  rows with a pair than `tessera.join_bloom_ratio`, a table of 4096 rows
  at least), every probe row costs `join_bloom_test_cost` instead and
  only the rows with a pair the probe;
- the pairs: `join_pair_cost` a pair, `join_gather_cost` more for every
  inner integer gathered and `join_text_value_cost` for every inner
  by-reference value, `join_compact_pair_cost` more where the inner keys'
  distinct values, from the statistics, give several records a key, since
  the pairs then go out in compact batches; the residual clauses cost
  `filter_clause_cost` a clause over the records matched;
- the batches: `join_batch_cost` a batch the node publishes, since an
  outer batch with a pair goes out as a round whatever the share of its
  rows selected and the parent pays for it (one match in ten costs a
  batch for 6 rows): the outer batches with a pair at the expected share,
  or the compact batches of the pairs;
- spilling: `join_spill_row_cost` a row of either side once the table, at
  16 bytes a record, 8 a key, 8 for the NULL bits of the columns kept and
  8 a column, and 8 of the index a record, outgrows `hash_mem`, every
  participant's with a shared table.

A semi join's pairs are its rows, an anti join's the probe rows with a
record, a left join's its rows with every outer batch published. A
partial path's rows are one participant's share and its cost that
participant's time; with a shared table the build divides among the
participants. Where the join prunes its outer `TessAppend`, the outer
child's cost is scaled down by the partitions expected to be left.

The sum goes into the cost divided by `tessera.join_cost_unit`: the
planner weighs this path against the core's hash join of the same
inputs, whose cost the core counts by its nominal constants, at 4.9 µs
of its real time a unit of its cost over the same joins, where a unit of
the scan model is 0.27 µs. Without the division the node's time lost to
the core's underpriced hash join on every small table, though the node
runs those joins 3 to 5 times faster. The core's rate is not uniform (0.9
to 7.5 µs a unit by the join's shape: it underprices its build the most,
159 ns a row against its 0.0125 units), so one rate keeps the node's
joins ordered by their time among themselves and the node's join below
the core's wherever the node is at least 1.5 times as fast, which the
slowest shape measured is by 2.7.

| Parameter | Default | The node's time for |
|---|---|---|
| `tessera.join_build_cost` | 0.107 | a row of the inner side built into the table |
| `tessera.join_probe_cost` | 0.0072 | a row of the outer side probed |
| `tessera.join_pair_cost` | 0.0046 | a pair returned |
| `tessera.join_batch_cost` | 0.31 | a batch published, which its parent pays for whatever the share of its rows selected |
| `tessera.join_gather_cost` | 0.0097 | an inner integer gathered for a pair |
| `tessera.join_text_value_cost` | 0.035 | a by-reference inner value copied into the table, and again gathered for a pair |
| `tessera.join_hashed_key_cost` | 0.069 | a row of either side hashed by its type's function, for a key a word does not hold |
| `tessera.join_compact_pair_cost` | 0.020 | what a pair of a compact batch costs more, where the inner side has several records a key |
| `tessera.join_bloom_test_cost` | 0.0027 | a probe row tested against the Bloom filter, in place of the probe of a row it rejects |
| `tessera.join_spill_row_cost` | 0.015 | what a row of either side costs more once the table outgrows `hash_mem` |
| `tessera.join_cost_unit` | 17.9 | the node's time, in these units, that a unit of the core's hash join cost stands for: the node's own time is divided by it |

Measured by `bench/pg/joincost` (pg-joincost-0EsSrQ, xClcRS): a row
built 28.6 ns, a row probed 1.9, a pair 1.2, a batch 83, an integer
gathered 2.6, a text value 9.5, a hashed key 18.6 a row, a compact pair
5.5 more, a Bloom test 0.7 (the bits of 100 000 inner rows cost nothing
the twins with and without the filter show), a row of a spilling join
4.0 (the excess grows with the rows, not with the share of the table
past `hash_mem`); the model predicts the 52 serial samples within 12 %
root mean square, the parallel ones within 20 % of one participant's
share. The core's hash join took 159 ns a row built, 4.2 a row probed
and 25 a pair, 4.9 µs a unit of its cost over the base's queries.

## Aggregates

`TessAgg` groups and aggregates a batch child's rows, the kernels folding
its own aggregates (`count`, `sum`, `avg`, `min`, `max` over the types the
batches hold as words, with sum states for `numeric` and `bigint`) and
the core's transition functions running row by row for the others (see
[nodes.md](nodes.md), TessAgg). The work is the core's kind, a transition
a row, so its cost is a share of what the core counts
(`get_agg_clause_costs`), over the child's cost as it is:

- a grouping (`group_cost`): per input row `agg_key_share` of
  `cpu_operator_cost` a key, since the kernels hash and look up a batch's
  keys at once, and `agg_dictionary_share` more for a key a word does not
  hold, which goes through the dictionary (its type's hash and the
  lookup); the aggregates' transition costs as the core counts them,
  `agg_kernel_share` of them when the kernels fold every aggregate, the
  whole with a generic one; an argument the kernels do not compute at the
  core's cost of evaluating it a row; per group `cpu_tuple_cost` and the
  final costs; and, when the groups at the node's bytes an entry pass
  seven eighths of `hash_mem`, the rows of the groups that do not fit
  written and read once a level of partitioning, `seq_page_cost` a page
  and `cpu_tuple_cost` a row;
- an aggregation without `GROUP BY` (`plain_cost`): the transition costs
  at `agg_kernel_share` when the kernels fold every aggregate and at
  `agg_generic_share` with one they do not, an argument the kernels do
  not compute at the core's cost;
- the finalizing stage of a parallel aggregation: the core's cost as it
  is, since its work is a row a participant.

| Parameter | Default | The node's share of |
|---|---|---|
| `tessera.agg_key_share` | 0.25 | `cpu_operator_cost` a key of a row grouped |
| `tessera.agg_dictionary_share` | 0.65 | more of it for a key through the dictionary (text, numeric, ...) |
| `tessera.agg_kernel_share` | 0.25 | the core's transition cost a row of the kernels' own aggregates |
| `tessera.agg_generic_share` | 0.9 | the same for an aggregate the kernels do not fold, run row by row |

Measured: `SELECT DISTINCT` through the dictionary took 0.45 to 0.84 of
the core's hashed time over the same scan with text, varchar and char
keys (plan 5.13); over the same batch scan of 500 000 rows, `max` of a
text took the node 10.7 ms and the core's Aggregate 13.4 (0.80), three
aggregates with a text `max` 19.1 and 21.8 (0.87), `string_agg` over
10 000 rows 3.6 and 3.2 (1.13) (plan 8.10). At a share of 1 the node's
path ties the core's and loses, since `add_path` keeps the path it has.

## The other nodes

- **Set operations** (`INTERSECT`, `EXCEPT`, through TessAgg's second
  mode): the sides' batch paths and `setop_word_share` of what the core's
  SetOp costs over its own, `setop_dictionary_share` with a key through
  the dictionary: the node took 0.55 of the core's time with keys of
  words, 0.9 with a text key (plan 5.13).
- **TessPack**, which builds batches from a core child's rows: the
  child's cost and `pack_value_share` of `cpu_operator_cost` a value and
  one more a row: the builder took 7.8 ns a row of nine columns and 1.6
  of one where the core's aggregate above took 21 (plan 4.28); forwarded
  batches cost nothing.
- **TessGather** and **TessGatherMerge**: the core's gather over the
  same partial path, less three quarters of `parallel_tuple_cost` a row
  (`gather_tuple_share`): 13.3 M rows took the leader at most 3.5 ns each
  through the node against 14 through the core's `Gather`.
- **TessAppend**: the core's Append over the same children, less the
  core's cost of a row through an Append, which the node saves by handing
  batches on.
- **TessSort** and **TessLimit**: the core's path's cost as it is; the
  node's sort is faster, but a sort under a parent of the core that takes
  rows one at a time is not, and the two cases are not told apart in the
  cost (plan 4.28, 4.29).

| Parameter | Default | The node's share of |
|---|---|---|
| `tessera.setop_word_share` | 0.5 | the core's own cost of `INTERSECT` or `EXCEPT` with keys of words |
| `tessera.setop_dictionary_share` | 0.9 | the same with a key through the dictionary |
| `tessera.gather_tuple_share` | 0.25 | `parallel_tuple_cost` a row through TessGather |
| `tessera.pack_value_share` | 0.4 | `cpu_operator_cost` a value TessPack copies into a batch |

## Calibration

Two families of `bench/pg` measure the models' prices; they calibrate,
they do not compare. Each runs on the benchmark cluster of
`bench/pg/run.sh` (`setup` loads the tables, `measure <family>` runs one
and writes a directory under `target/bench-runs/` with the samples, the
fit and `summary.txt`), takes the minimum of seven runs of every query
after two warm-ups, and prints the parameters' values in the models'
units, each sample against its prediction and the error over all:

- `scancost`: full scans with a filter over six tables of other widths,
  the index-only scan, the index mode and the bitmaps of an ordered and
  a scattered column at 1 to 50 % of the rows, the full scans with
  clauses past the first (in batches, by rows, past a varlena), the full
  scans with two workers with and without the leader and a parallel
  count of a tiny table for the workers' start, the parallel bitmaps and
  index scans; and the core's scans with Tessera off, as time per unit
  of their cost.
- `joincost`: hash joins of a fact table against inner sides of 10 000
  to 2 000 000 rows, each over every outer row and over the half, so
  that the rows of the two sides and the pairs vary apart; the node's
  own time is the query's less its children's, the same scans timed
  alone with the same columns read. The samples vary the inner side's
  size, the keys' types, the inner columns kept, several records a key,
  a residual clause, semi, anti and left joins, the Bloom filter with
  and without, spilling and a shared table with two workers; the base's
  queries run again with Tessera off for `join_cost_unit`. The family
  stops with an error where the planner did not take the node, so it
  must run against a build whose model does.

To set the parameters on a machine: run the families there, then put the
values the summary prints into `postgresql.conf` or `ALTER SYSTEM`, or
`SET` them in a session; a value given before the module loads is kept
(see [bridge.md](bridge.md)). The families' method has limits of its
own: a sample's children are subtracted from its time, and a reference
scan with a clause can cost a millisecond more than the full one, which
leaves the smallest samples short; a run on a busy machine drifts, so a
run compares its parameters with the previous one's (the joincost
parameters stayed within 5 % over five runs on an idle machine); and a
machine's timer, caches and memory bandwidth are its own, which is why
the defaults are the project's and not a deployment's.

## Known limits

- The core's own costs are nominal, and the comparison of a node's path
  with a core node of another kind is as good as the core's consistency
  with itself: the core's sequential scan took 0.92 µs a unit of its
  cost, its bitmap 0.57, its index-only scan 0.74, its hash join 4.9
  (scancost, joincost). The hash join's rate is converted; the others
  are not, so where the core weighs a nested loop over an index against
  the node's hash join, the node's time is honest and the core's is the
  core's (Q8 of TPC-H: the core's nested loops over `customer` and
  `supplier` take two of its joins, 10 % slower than the node's).
- The rows are the core's estimates: a join the core expects 98 rows of
  where 319 404 come (Q9 of TPC-H) misleads every path alike.
- The spill models count a row once the table spills, not the share on
  disk; a table near the limit may spill or not by the memory accounting
  of the moment.
- A partial path costs one participant's share; a shared table's build
  took 30 % more per row than a table of one process, which the model
  leaves out, and the workers' start is the scans' parameter.
- A table past `effective_cache_size`, whose pages may come from the
  disk, keeps the core's costs, and so does a relation whose clauses all
  run row by row.
