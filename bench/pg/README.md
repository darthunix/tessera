# PostgreSQL-level benchmarks

These benchmarks measure whole queries on a temporary PostgreSQL cluster
with the Tessera modules preloaded into the server, with `tessera.enable` on and off in one
session, so that the batch plan and the core plan run on the same data in
the same backend. They answer one question at a time: does Tessera slow a
query down, and where it can, does it speed one up.

## Families

- **tax** (`tax.sql`): queries where a batch plan cannot win, such as LIMIT
  above a scan, a count without a filter, a sorted limit. The ratio of
  Tessera on to off measures the cost of the batch boundary itself; it
  should stay close to one.
- **win** (`win.sql`): filters over large tables with aggregates above
  them, through `TessAgg` above `TessFilter` above `TessHeapScan`, and two
  aggregate cases without a filter or with four aggregates over one
  column. A ratio below one is the win; each run is compared with the
  previous one.
- **join** (`join.sql`): equi-joins over one integer key between a fact
  table of 2 M rows and a dimension of 100 k, or a table with four rows
  per key (also with a text outer column): a count, a column of either side, int8 keys past the int4
  range and an int4 key against an int8 one, two keys and a residual join
  clause over columns of both sides, a dimension filtered to one key in a
  hundred and a key that matches nothing, a top-N sort above the
  join and a join over a join; then where the core may merge: a full and
  a right join, which the batch hash join does not do, a join with hash
  joins disabled (`measure_setting`), and sides already in the order of
  their keys through their indexes (`bench_mj_outer`, `bench_mj_inner`);
  the partitioned `bench_part` keyed by the join key against every key of
  the dimension and a thousand of them, which reach one partition of
  four. With Tessera on, `TessHashJoin` builds the
  inner side and probes it with batches of the outer side, under `TessAgg`
  where the query aggregates; with it off, the core's `Hash Join`. A ratio
  below one is the win. `plan_time` plans a join of four relations
  without running it, 100 plans per sample, and reports the mean: the
  cost of the join hook, which must stay close to one.
- **probe** (`probe.sql`): where the time of a parallel query goes. A
  table of 28 pages under a `Gather`, serially and with one and two
  workers, with and without the leader taking part: all its time is the
  fixed cost of launching and finishing the workers, and any difference
  between the modes is the batch nodes' own. Then `count_all` and `dense`
  of the win family with one, two and four workers, `mixed` and `wide`
  with two, and the plans of both modes with `EXPLAIN VERBOSE`, which
  shows every worker's time and rows under each node. Run with
  `measure probe 2`; the workers of each case are set in the file.
- **spill** (`spill.sql`): joins and groupings whose table outgrows
  `hash_mem` at the data multiplier 10 (`SHARED_BUFFERS=4GB run.sh setup
  10`), each at a `work_mem` of 4, 16 and 64 MB, set while both modes plan
  and run it: the fact table's key into the dimension as an inner join
  with a dimension column, a left join and `NOT EXISTS`; the dimension's
  keys against `bench_mixed` with its text column on the inner side; the
  fact table grouped by its key (1 M groups) and `bench_mixed` by its
  unique one (5 M). A case runs for seconds, so it is warmed up twice
  and timed 11 times (`-v warmups=`, `-v repetitions=` change that); the
  plans record the partitions and disk of both modes. Run serially and
  with `measure spill 2`.
- **sort** (`sort.sql`): `ORDER BY` over `bench_sort` (2 M rows per
  multiplier, keys in no order of the rows): a unique int4 key, an int8 one
  past the int4 range, a key of 1000 values, two keys, a descending key with
  NULLs last, a key already in the order of the rows, a text column carried
  along, and a sort over a filter keeping one row in ten; each skips every
  row with `OFFSET`, so the sort's whole output is read and nothing is
  returned. Two more count an int and a text column of the sorted rows
  above them, so that the sort's columns are read too; four take the first
  rows under `LIMIT` (top-N): 10 and 100000 rows of keys in no order, 10
  rows of keys in the reverse of the rows' order, where every row beats
  the ones kept so far, and 10 rows after an offset of 1000; four count
  distinct values: `SELECT DISTINCT` of a key of 1000 values and of a
  unique one, `count(DISTINCT)` over the table and per group of 1000;
  five sort by keys of other types: numeric, text under `"C"` (their
  words are their abbreviated keys), both also as the first 10 rows
  under `LIMIT`, and text of the default collation after the key of 1000
  values. `work_mem` is 512 MB in both modes, so the
  core sorts in memory. A ratio below one is the win.
- **setop** (`setop.sql`): `UNION ALL` and `UNION` over batch scans: an
  aggregate over two filtered scans, a hash join whose outer side is a
  `UNION ALL`, an aggregate over the four range partitions of
  `bench_part`, a `UNION ALL` returned as rows and skipped with `OFFSET`,
  and `UNION` of 1000 and of 2 M distinct values, and of 1000 values the
  planner's statistics know (`setop_known`, which workers split where
  the others stay serial by their estimates), and a `UNION` as the left
  side of `EXCEPT` (`setop_nested`). With the core's
  `Append` a batch parent packs the rows of batch children again. Four
  more group both sides of `INTERSECT` and `EXCEPT`: `EXCEPT` of 500 000
  integers and a third of them, `EXCEPT ALL` of 1000 values with many
  copies, `INTERSECT` of texts through a dictionary, `INTERSECT ALL` of
  integers. A ratio below one is the win.
- **index** (`index.sql`): reads through the indexes of `bench_idx` (2 M
  rows per multiplier, `k` scattered over the pages, `w` of 97 values,
  a btree of `id` and a BRIN index of the day `d`, both in the order of the
  rows): bitmaps of `k` at 1,
  5, 15 and 30 % of the rows, BitmapAnd of `k` and `w`, BitmapOr of them,
  and the rows of a bitmap skipped by a limit's offset; index scans of the
  ordered `id`: 10, 3 and 50 % of the rows aggregated, 5 % skipped by an
  offset, a range in the index's order, a range with a clause on another
  column; index-only scans of `k` and `w`: 10, 1, 20 and 50 % counted, summed,
  skipped by an offset, in the index's order to a limit, grouped; BRIN
  bitmaps of the day: a week, a month and five months aggregated, 20 days
  skipped by an offset. With Tessera the rows come in batches and a filter
  rechecks every clause, dates too. At 30 and 50 % the full scan is the
  faster, which the planner's choice shows. A ratio below one is the win.
- **scancost** (`scancost.sql`): not a comparison but the calibration of
  the planner's model of the node's scans: the full scan with a filter
  over six tables of 30 to 140 rows a page, the index-only scan, the
  index mode and bitmaps of the ordered `id` and the scattered `k` of
  `bench_idx` at 1 to 50 % of its rows, and the core's scans with Tessera
  off. The summary fits the times (a page and a row of the full scan, a
  row of either index scan, a page, a row and a scattered row of a
  bitmap, the core's time per unit of its cost), prints the parameters
  `tessera.scan_page_cost` and the rest they give, a full scan's page
  being 1, and each sample against its prediction. Full scans of
  `bench_mixed` and `bench_narrow` with clauses past the first (in
  batches and by rows, on a column past a varlena, on every row and on
  half) fit the model of the filter and print
  `tessera.filter_clause_cost` and the rest. The full scans run
  again with two workers, without the leader and with it, and a count of
  `bench_tiny` times the workers' start and finish: they fit the model of
  a partial scan (the start, a worker's toll a page, the leader's head
  start) and print `tessera.scan_parallel_setup_cost` and
  `tessera.scan_worker_page_cost`. Run it on another machine to set the
  parameters there; the model is described in
  [docs/costs.md](../../docs/costs.md).
- **joincost** (`joincost.sql`): not a comparison but the calibration of
  the planner's model of TessHashJoin (`tessera.join_build_cost` and the
  rest, docs/nodes.md). Every sample is a hash join of `bench_fact`, or
  `bench_tfact` for text and numeric keys, against an inner side of
  10 000 to 2 000 000 rows, run over every outer row and over the half
  with `f1 <= 1000000`, so that the rows of the two sides and the pairs
  vary apart; the node's own time is the query's less its children's,
  the same scans timed alone with the same columns read. The samples vary
  the inner side's size, the keys' types, the inner columns kept
  (integers and a text), four records a key, a residual clause, semi,
  anti and left joins, the Bloom filter (the selective joins with it and
  without), spilling at a `work_mem` of 4 and 16 MB and a shared table
  with two workers. The summary fits the base (a row built, a row probed,
  a pair, a batch published) over the samples with integer keys and
  nothing kept, then each other kind of work from its samples' excess
  over the base (a gathered integer, a text value, a hashed key, a pair of
  a compact batch, a Bloom test, a spilled row), prints the parameters in
  the units of the scan model, every sample against the whole model with
  its error, and the parallel samples against one participant's share.
  The base's queries run again with Tessera off: the core's hash join's
  own time per unit of its own cost, sample by sample and over all, is
  `tessera.join_cost_unit`, the rate the planner converts the node's time
  at. Run it on another machine to set the parameters there; the model is described in
  [docs/costs.md](../../docs/costs.md).
- **rowwise** (`rowwise.sql`): tables without clauses read under a parent
  of the core that takes rows one at a time: `bit_or` of a column and of
  an expression, one column of sixty, `max` of a text column, a window
  function over every row, and the first row under `LIMIT 1`. With
  Tessera on, the native scan serves the rows; with it off, the core's
  sequential scan. A ratio below one is the win.
- **wordkey** (`wordkey.sql`): keys of types the hash table and the sort
  keep in a word besides int4 and int8, over `bench_dates` (2 M rows): a
  grouping by a date, by a timestamp and by an int2 and a boolean, a join
  on the date with the 1826 days of `bench_days`, `DISTINCT` of the
  timestamp and a sort by the date skipped with `OFFSET`. A ratio below
  one is the win.
- **anyagg** (`anyagg.sql`): aggregates without `GROUP BY` that `TessAgg`
  computes through the core's transition and final functions over the
  batches: `max` of text, `sum` and `avg` of int8, `sum` of numeric, `avg`
  of float8, `bit_or` over 2 M rows, `max` of text over a filter and three
  aggregates together. A ratio below one is the win.
- **anykey** (`anykey.sql`): groupings, `DISTINCT` and `UNION` keyed by
  text and numeric over `bench_mixed`: a text expression of 99 values, a
  text column of 450 000, with aggregates, numeric of 1000 values, and
  `DISTINCT` and `UNION` of text prefixes. Their values get numbers
  through a dictionary by the type's hash and equality. Hash joins of
  `bench_tfact` (2 000 000 rows) with `bench_tdim` (100 000) by text, by
  numeric, by text and an integer, and a semi-join by text: the table
  keeps the 64-bit hashes of the values, and the equality decides each
  pair. A ratio below one is the win.
- **exec** (`exec.sql`): the execution costs of plan item 4.27 (review
  section G), at 11 repetitions: rows a sort (eight columns) and a
  grouping serve one at a time to a window function of the core; ORDER BY
  text whose first nine bytes are the same, under an ICU collation and
  under "C", and an ICU control whose abbreviated keys tell rows apart
  (`bench_prefix`, 1 000 000 rows, made by the family); a join at a
  work_mem of 1 MB whose outer keys are half one key (`bench_skew`, made
  by the family) and a control without the skew; and the planning alone
  of thirty aggregates of expressions (EXPLAIN, unprepared). A ratio below
  one is the win.
- **oltp**: short prepared-statement queries, `pgbench -S`, where the
  planner hooks and the loaded modules must not slow down queries that
  never use a batch node. Arrives when more than one hook is installed.

## Rules

Wall time is the metric: the backend's performance counters need root and
PostgreSQL does not run as root. To make time trustworthy, every case is
executed five times as a warm-up and then 31 times, each timed with
`clock_timestamp()` around `EXECUTE`; the report gives the minimum, the
median and the 10th and 90th percentiles, and the on/off ratio of the
minimum and of the median. Each statement is prepared twice, once per
mode, because a prepared statement is planned at its first execution and
cached. `jit` is off, autovacuum is off, the tables are warm in
`shared_buffers`. Parallel query is off unless a number of workers per
`Gather` is given; then both modes run with it, and the on plan must show
the batch nodes under the `Gather` with that many workers launched. The machine is idle and the power source is
recorded; nothing is retried or discarded, and a bad run is kept and
explained. The plans of both modes are recorded with `EXPLAIN (ANALYZE,
VERBOSE)`, which the batch nodes' counters need, and checked by eye: the
on plan must contain the batch nodes, the off plan must not.

## Running

From the repository root, with the modules installed into the PostgreSQL
of `pg_config` (`make install`):

```sh
bench/pg/run.sh setup          # temporary cluster on port 5433 with data
bench/pg/run.sh measure tax    # one family; writes target/bench-runs/pg-tax-<id>/
bench/pg/run.sh measure win 2  # the same with two parallel workers per Gather
                               # in both modes; writes pg-win-w2-<id>/
CASES='^ix_' REPETITIONS=11 bench/pg/run.sh measure index
                               # only the cases the pattern matches, 11 runs each
bench/pg/run.sh stop           # stop and delete the cluster
SHARED_BUFFERS=4GB bench/pg/run.sh setup 10   # ten times the rows, larger buffers
```

`CASES`, a regular expression of case names, times only the cases it
matches (the plans are still written for every case), and `REPETITIONS`
the runs of each case in each mode, 31 by default: a development A/B times
the cases a change touches, fewer times, so that a run takes minutes at
most. [`cargo ab measure`](../../tools/tessera-ab/README.md) runs such an
A/B of two revisions, installing each in turn and comparing every case
with the control without Tessera. `PG_CONFIG` selects the PostgreSQL build; `PGPORT` the port. The cluster's
`postgresql.conf` preloads `tessera, tessera_nodes, tessera_kernels`
into the postmaster (`shared_preload_libraries`, so that
parallel workers do not load them again in every query) and sets
`shared_buffers` to `SHARED_BUFFERS`, 2GB by default; `measure` restarts
the server before it runs, so that the modules installed last are the ones
measured, and every case's warm-up runs bring its tables back into
the buffers. `setup` takes a multiplier of the base row counts (2 M narrow,
250 k wide, 500 k mixed, 100 k dimension, 2 M fact, 100 k with duplicate
keys), stored in the table `bench_scale`; the win and join families
multiply their selection constants and key ranges by it, so that every case keeps its
selectivity, while the tax family's LIMIT constants are the cases themselves
and it runs at the base size only.

## Results

Each measurement gets a directory under `target/bench-runs/` (ignored by
git) with `protocol.md` (this file's rules at the time), `source/` (the SQL
that ran), `source.txt` (the revision, the number of workers, the data multiplier and
`shared_buffers`, the working tree's status and the hashes of the installed
libraries and `postgres`), `power.txt`,
`timings.csv` (every sample), `summary.txt` (the table) and `plans.txt`.
Write `comparison.md` by hand: what was measured, the table with ratios,
observations, and what the numbers mean for the tolerance of the family;
the tolerance itself is set from data, never in advance.
