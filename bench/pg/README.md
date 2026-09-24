# PostgreSQL-level benchmarks

These benchmarks measure whole queries on a temporary PostgreSQL cluster
with the Tessera modules preloaded, with `tessera.enable` on and off in one
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
  per key: a count, a column of either side, int8 keys past the int4
  range and an int4 key against an int8 one, two keys and a residual join
  clause over columns of both sides, a dimension filtered to one key in a
  hundred and a key that matches nothing, a top-N sort above the
  join and a join over a join. With Tessera on, `TessHashJoin` builds the
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
explained. The plans of both modes are recorded with `EXPLAIN ANALYZE`
and checked by eye: the on plan must contain the batch nodes, the off plan
must not.

## Running

From the repository root, with the modules installed into the PostgreSQL
of `pg_config` (`make install`):

```sh
bench/pg/run.sh setup          # temporary cluster on port 5433 with data
bench/pg/run.sh measure tax    # one family; writes target/bench-runs/pg-tax-<id>/
bench/pg/run.sh measure win 2  # the same with two parallel workers per Gather
                               # in both modes; writes pg-win-w2-<id>/
bench/pg/run.sh stop           # stop and delete the cluster
SHARED_BUFFERS=4GB bench/pg/run.sh setup 10   # ten times the rows, larger buffers
```

`PG_CONFIG` selects the PostgreSQL build; `PGPORT` the port. The cluster's
`postgresql.conf` preloads `tessera, tessera_nodes, tessera_kernels,
tessera_limit` in every session and sets `shared_buffers` to `SHARED_BUFFERS`,
2GB by default. `setup` takes a multiplier of the base row counts (2 M narrow,
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
