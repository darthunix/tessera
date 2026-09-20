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
- **win**: filters and aggregates over large tables, where a ratio below
  one is expected once a native batch scan exists. Arrives with that scan.
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
cached. `jit` is off, parallel query is off, autovacuum is off, the tables
are warm in `shared_buffers`. The machine is idle and the power source is
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
bench/pg/run.sh stop           # stop and delete the cluster
```

`PG_CONFIG` selects the PostgreSQL build; `PGPORT` the port. The cluster's
`postgresql.conf` preloads `tessera, tessera_nodes, tessera_limit` in every
session and sets `shared_buffers = 2GB`.

## Results

Each measurement gets a directory under `target/bench-runs/` (ignored by
git) with `protocol.md` (this file's rules at the time), `source/` (the SQL
that ran), `source.txt` (the revision, the working tree's status and the
hashes of the installed libraries and `postgres`), `power.txt`,
`timings.csv` (every sample), `summary.txt` (the table) and `plans.txt`.
Write `comparison.md` by hand: what was measured, the table with ratios,
observations, and what the numbers mean for the tolerance of the family;
the tolerance itself is set from data, never in advance.
