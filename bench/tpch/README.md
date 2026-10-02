# Queries derived from TPC-H

`cargo tpch` runs the 22 queries of TPC-H on a PostgreSQL cluster of its
own, once with Tessera and once without, and answers three questions per
query:

1. **Is the answer right?** With Tessera off it must match the published
   answer of TPC-H; with Tessera on it must match the answer with Tessera
   off, value for value.
2. **How much of the query does Tessera run?** Which Tessera nodes the plan
   has, and what share of the rows read from the tables went through
   Tessera's scan.
3. **How much faster or slower is it?** The median time with Tessera on
   over the median with it off, with a confidence interval.

It is a tool of the repository, `tools/tessera-tpch`, run by hand; CI only
builds and unit-tests it.

## Quick start

```sh
export PG_CONFIG=$HOME/postgres/bin/pg_config   # a build of PostgreSQL master
cargo tpch                                       # or: make tpch
```

The first run builds the release libraries of Tessera and installs them
into the build of `PG_CONFIG`, creates a cluster in
`target/bench-runs/pgdata-tpch-sf1` (port 5434), generates and loads the
data at scale factor 1 (8 seconds, 1.5 GB), checks every answer, times the
queries and prints a table. Before the timing it prints how long it will
take. The next runs find the cluster and the data and skip to the check.

Time the queries on an idle machine on mains power: the times are wall
clock. `cargo tpch check` times nothing and may run on a busy machine.

**What it needs:**

- a PostgreSQL master build with `pg_config`, the one Tessera builds
  against;
- the contrib modules `pg_prewarm` and `pg_buffercache` installed in that
  build. If they are missing, the tool stops and prints the commands, such
  as `make -C $HOME/postgres/build/contrib/pg_prewarm install`;
- memory for `shared_buffers`: 2 GB at SF 1, 16 GB at SF 10 (1.6 GB per
  scale unit, at least 2 GB);
- disk: about 3 GB at SF 1 with the WAL, ten times that at SF 10.

## Commands

| Command | What it does |
|---|---|
| `cargo tpch` or `cargo tpch run` | the check, then the timing, then the table |
| `cargo tpch check` | the answers and the plans only, no timing |
| `cargo tpch setup` | install Tessera, start the cluster, load the data; the server keeps running for `psql -h /tmp -p 5434 tpch` |
| `cargo tpch report <dir>` | print a saved run again |
| `cargo tpch compare <A> <B>` | compare two saved runs query by query |
| `cargo tpch stop` | stop the cluster; its data stays |

`check` and `run` stop the server at the end if they started it, and leave
it running if it ran before. To remove a cluster, stop it and delete its
directory.

The Makefile has the same as `make tpch`, `make tpch-check` and
`make tpch-stop`; `TPCH_SF` sets the scale factor and `TPCH_FLAGS` adds
flags:

```sh
make tpch-check TPCH_SF=10
make tpch TPCH_FLAGS="--queries core --pairs 5"
```

## Flags

| Flag | Default | Meaning |
|---|---|---|
| `--sf N` | 1 | scale factor; each has its own cluster |
| `--queries LIST` | all | numbers and ranges (`1,3,6`, `1-5`), `core` (Q1, Q3, Q6, Q9, Q18) or `all` |
| `--schema pk\|indexed` | pk | primary keys only, or with indexes on foreign keys and dates (`indexes.sql`) |
| `--workers N` | 0 | `max_parallel_workers_per_gather` in both modes |
| `--work-mem SIZE` | 256MB | `work_mem` in both modes |
| `--jit` | off | run with `jit = on` |
| `--timeout SECONDS` | 30 per scale unit, at least 30 | `statement_timeout` of a query |
| `--pairs N` | 11 | timed pairs per query (`run`) |
| `--warmups N` | 1 | untimed executions per mode before the pairs (`run`) |
| `--threshold PERCENT` | 3 | a ratio closer to one than this counts as even (`run`) |
| `--no-install` | | keep the Tessera build already installed |
| `--reload` | | generate and load the data again |
| `--shared-buffers SIZE` | by scale | `shared_buffers` of the server |
| `--port N` | 5434 | port of the server |
| `--keep-running` | | leave the server running at the end |
| `--update-golden` | | write the plans into the golden file instead of comparing them |

## Reading the table

```
│ query                     ┆ answer ┆ Tessera nodes ┆ rows via Tessera ┆ off, ms ┆ on, ms ┆ on/off ┆ 95 % interval │
```

- **answer**: `same` when the answer matched the published one (at SF 1)
  and Tessera on matched off; `MISMATCH` with the first different row,
  `ERROR` with the server's message, `TIMEOUT` when the query ran past
  `--timeout`. Only `same` queries are timed.
- **Tessera nodes**: how many nodes of the plan with Tessera on are
  Tessera's (`TessHeapScan`, `TessFilter`, `TessHashJoin`, `TessAgg`,
  `TessSort` and the rest).
- **rows via Tessera**: of all rows the scans read from the tables, the
  share that Tessera's `TessHeapScan` read. 100 % means no table was read
  by PostgreSQL's own scans.
- **off, ms** and **on, ms**: the median time of the query with Tessera off
  and on.
- **on/off**: the ratio of the two medians. Below one Tessera is faster:
  0.50 is twice as fast. Green when faster by more than `--threshold`, red
  when slower by more.
- **95 % interval**: where the ratio lies with 95 % confidence, from a
  bootstrap over the pairs. An interval that crosses one means no
  difference was shown.

Below the table: how many answers were the same and which queries ran out
of time; the geometric mean of the ratios over all timed queries and over
those with Tessera nodes; the sums of the medians; and how many queries
were faster, slower or even.

## How it works

**The build.** Unless `--no-install`, every command runs
`make RUST_PROFILE=release` and `make install` against `PG_CONFIG`, so
that a debug build left installed by a test run is never measured. A
running server is restarted, since it holds the modules it loaded.

**The cluster.** One per scale factor, initialised in the C locale (the
published answers order strings by bytes), with the Tessera modules in
`shared_preload_libraries`, `autovacuum` off and `jit` off. The settings a
query runs under are set per connection, so changing them needs no
restart.

**The data.** The `tpchgen` crate generates the rows in the process; they
are those of the reference generator dbgen byte for byte. Each table is
copied through a connection of its own, all tables at once, so that its
rows lie in the order of their keys, the same at every load. Then the
primary keys, `VACUUM (FREEZE, ANALYZE)` and a second `VACUUM` (on
PostgreSQL master the first one after a `COPY` marks no page all-visible),
and the row count of every table against the specification. The schema
is in `schema.sql`, `keys.sql` and `indexes.sql`.

**The check.** Two connections, one with `tessera.enable = on` and one
with `off`, each with every query prepared once (`qNN.sql`, with the
validation parameters of the specification). Each query runs once in each
mode. With Tessera off its answer is compared with the published answer
at SF 1 by the precision rules of the specification (clause 2.1.3.5):
counts and single values exactly, sums within 100, averages and ratios
within 1 % after rounding to 0.01. With Tessera on its answer is compared
with off byte for byte. Rows are compared in order, except that rows equal
in the `ORDER BY` columns may come in any order, and where `LIMIT` cuts
such a group only their sort columns are compared.

**The plans.** Every query that answered runs once more in each mode
under `EXPLAIN (ANALYZE, TIMING OFF, FORMAT JSON)`. With Tessera off a
Tessera node fails the check. With Tessera on the nodes and the share of
rows are compared with the golden file `participation-sf1.txt`: the
planner's choice does not depend on the machine's noise, so a change in it
is a signal. A difference is printed, not failed; after a change that
moves plans on purpose, `cargo tpch check --update-golden` rewrites the
file.

**The timing.** `pg_prewarm` reads every table and index into shared
buffers, and `pg_buffercache` confirms they are there; a plain scan of a
table larger than a quarter of shared buffers would not keep it there.
Then, query by query, through the same connections and prepared
statements: `--warmups` untimed executions per mode, then `--pairs` pairs
of one execution per mode, Tessera on first in even pairs and off first in
odd ones (ABBA), so that a drift of the machine falls on both modes. The
time is the client's, from sending the query to its last row.

## The files of a run

Each `check` and `run` writes `target/bench-runs/tpch-sf<N>-<id>/`:

| File | Contents |
|---|---|
| `source.txt` | commit and changed files, SHA-256 of the installed libraries and of `postgres`, power source, load average, table sizes |
| `pg_settings.txt` | every setting of the connection with Tessera on |
| `results.txt` | per query: rows, the verdicts, the time of the single execution in each mode of the check, the first difference |
| `participation.txt` | per query and mode: rows read, rows via Tessera, rows the batch filters removed, Materialize nodes over Tessera nodes, the Tessera nodes |
| `plans/qNN-on.json`, `qNN-off.json` | the plans of `EXPLAIN (ANALYZE, TIMING OFF)` |
| `timings.csv` | every timed execution: query, pair, mode, position in the pair, ms, rows |
| `summary.md` | the table and the totals in Markdown |
| `run.json` | all of it, which `report` and `compare` read |

## Comparing two builds of Tessera

```sh
git switch base-branch
cargo tpch --queries core --pairs 5
git switch my-change
cargo tpch --queries core --pairs 5
cargo tpch compare target/bench-runs/tpch-sf1-<first id> target/bench-runs/tpch-sf1-<second id>
```

Each run installs the build of the checked-out tree. `compare` shows, per
query, the time with Tessera on in B over A with its interval, and the
time with Tessera off in B over A: the core is the same in both runs, so
that column shows how much the machine itself drifted between them.

## Other settings

- `--workers 2`: parallel plans in both modes.
- `--schema indexed`: the second variant of the schema. Its indexes on
  `lineitem (l_partkey, l_suppkey)` and others let Q17 and Q20 finish,
  and bring in the index scans of Tessera.
- `--sf 10`: 16 GB of shared buffers and a time limit of 300 s. There are
  no published answers at SF 10, so only on against off is checked.
- `check --work-mem 4MB --workers 2`: the answers when joins and
  aggregates spill to disk and run in parallel.
- `--jit`: one control run with PostgreSQL's JIT, which the queries at SF 1
  and above reach.

## Known limits

- With the primary keys only, Q17 and Q20 run past the time limit in both
  modes: their correlated subqueries read `lineitem` once per outer row,
  with no index on `l_partkey`. `--schema indexed` gives them one.
- The copy of Q11's answer in `tpchgen` 3.0 lost the last two digits of
  every `ps_partkey` (129760 is 1297 there; the values agree). The check
  drops them from our keys of Q11 too before comparing.
- The published answers exist at SF 1 only.

## Derived from TPC-H

These are queries and data of the TPC Benchmark™ H, run differently from
what its specification requires, and the results are not comparable to
published TPC-H results. They differ from it in that the data come from
`tpchgen` rather than DBGen (the same rows); the queries take the fixed
validation parameters rather than random ones; there are no refresh
functions, no throughput test and no QphH metric; Q15 is the variant with
`WITH`; and by default the tables have primary keys and no other index.
