# ClickBench over its Parquet files

`cargo clickbench` runs the 43 queries of
[ClickBench](https://github.com/ClickHouse/ClickBench) over its Parquet
files, read in place, on a PostgreSQL cluster of its own, in three
modes:

- **on**: the foreign table `fdw.hits` of the wrapper `tessera_parquet`,
  with Tessera's nodes reading the wrapper's batches;
- **off**: the same table with `tessera.enable = off`, PostgreSQL's
  executor reading the wrapper's rows;
- **duckdb**: pg_duckdb's view `ddb.hits` of `read_parquet` over the
  same files, so that DuckDB reads them, where the build has pg_duckdb.

It answers two questions: are the answers of the modes the same, and how
long does each query take in each mode, timed as ClickBench times it. It
is a tool of the repository, `tools/tessera-clickbench`, run by hand; CI
only builds and unit-tests it.

## Quick start

```sh
export PG_CONFIG=$HOME/pg19/bin/pg_config   # a build with pg_duckdb, or without
cargo clickbench check --files /data/clickbench
cargo clickbench run --mode on --queries 1-7 --files /data/clickbench
```

The first command builds the release libraries of Tessera and installs
them, with the wrapper, into the build of `PG_CONFIG`, creates a cluster
in `target/bench-runs/pgdata-clickbench` (port 5439), makes the tables
over the files, analyzes the foreign table, and checks the answers. The
next commands find the cluster and the tables, and make the tables anew
only when they are given other files.

The tree builds for one `PG_CONFIG`: after building it for another
PostgreSQL, run `make clean` first, or the objects of the other build
are installed.

**What it needs:**

- a PostgreSQL build Tessera builds against (master, 19 or 18), with
  pg_duckdb installed for the mode `duckdb`; without it the tool runs
  the other two;
- ClickBench's files. `hits.parquet`, 100 M rows and 14 GB, is
  `https://datasets.clickhouse.com/hits_compatible/hits.parquet`; the
  100 files of 1 M rows each are
  `https://datasets.clickhouse.com/hits_compatible/athena_partitioned/hits_<N>.parquet`
  for `N` from 0 to 99. `--files` takes files and directories, whose
  `.parquet` files are read in the order of their names; by default
  `target/clickbench-data` of the repository.

## Commands

- `cargo clickbench setup`: install Tessera, start the cluster, make
  the tables; the server keeps running for `psql -h /tmp -p 5439
  clickbench`, where `SET search_path = fdw` or `ddb, public` picks the
  table.
- `cargo clickbench check`: the answers, every query once in each mode,
  and the plans; no timing, so it may run on a busy machine.
- `cargo clickbench run --mode <on|off|duckdb>`: the times of one mode.
  Run it on an idle machine on mains power, after a check of the same
  files.
- `cargo clickbench report <dir>...`: a run again, or several side by
  side.
- `cargo clickbench stop`: stop the cluster; its data stays.

`check` and `run` stop the server at the end if they started it, and
leave it running if it ran before or with `--keep-running`. Both take
`--queries` (`all`, `1-7`, `1,3,24`, numbered from 1 as ClickBench's
results number them), `--workers` (`max_parallel_workers_per_gather`,
0 by default) and `--work-mem` (64 MB by default, as ClickBench's
PostgreSQL entry sets it).

## The tables

`create.sql` is the table of ClickBench's `postgresql/create.sql` as a
foreign table: the option `column` names each file column, whose names
are in mixed case; the option `unit` makes the integers of `EventTime`,
`ClientEventTime` and `LocalEventTime` seconds and those of `EventDate`
days, so that they are a `timestamp` and a `date`; `HitColor`, a `char`
there, is `text`, since the wrapper takes no `char(n)`. `view.sql` is
the same table for pg_duckdb, with the conversions of ClickBench's
`pg_ducklake/create.sql`. Both scripts run in psql too, with `\set
files` or `\set files_array`. `queries.sql` is ClickBench's
`postgresql/queries.sql` as it is.

## The check

Tessera off is the reference. Tessera on must give its answer value for
value. pg_duckdb must give it with its averages within a relative 10⁻⁹,
since DuckDB computes them in `double precision` and PostgreSQL in
`numeric`; every other column, value for value. Rows are compared in
their order, except that rows equal in the query's sort keys may come in
any order. Where a `LIMIT` cuts such a run of rows, or an `OFFSET` does,
any of the run's rows may be returned: Q18's ten groups have no order at
all, Q24's ten rows by `EventTime` share their second with others. There
the modes must agree on the count of the rows and on their sort keys,
and a companion query checks that the rows a mode returned are, with
their repeats, among the rows of the query without its `LIMIT` and its
`ORDER BY`, which Tessera off computes:

```sql
SELECT count(*) FROM (VALUES (...), (...) EXCEPT ALL
  SELECT f.c1::text, ... FROM (<the query without LIMIT>) AS f(c1, ...)) AS lacking
```

For pg_duckdb the companion compares its averages rounded to nine
significant digits on both sides, so in the rows of a cut run an
average of pg_duckdb passes within about 10⁻⁸ of a value of the full
result, and at a cut on an average, as Q28's, a row whose average is
that close to the last one passes in its place. The rounding may also
refuse two values that round apart, and two averages that `numeric`
tells apart and `double precision` does not may come from DuckDB in
either order and fail; such a check fails, to be looked at. Counts,
identifiers and text are compared exactly in every mode, and so is
every value of Tessera on. Three queries need more:

- Q25 sorts by `EventTime` and returns `SearchPhrase` alone, so its ten
  rows could be any ten phrases. The check runs it once more in Tessera
  off returning `EventTime` too, pairs each mode's row `i` with that
  answer's `EventTime` of row `i`, and compares the pairs by the sort
  key, their cut run through the companion: a right answer's times are
  those whatever rows of a tie it took, and a phrase of a later time is
  not in the full result beside an earlier one.
- Q42's `OFFSET 10000` skips every row over a small sample: where a
  query's `OFFSET` leaves no row, the check compares it without its
  `LIMIT` and `OFFSET` too.
- In Q29 DuckDB's regular expressions match no newline with `.`, and
  some values of `Referer` hold one, so a few rows fall in other groups:
  pg_duckdb's answer is compared with Tessera off's answer to the query
  whose pattern matches no newline there either, and reported as the
  same under DuckDB's rule.

Any other difference fails the check, as does an answer that errs or
times out (`--timeout`, 300 s a statement by default). It writes the
plans of every query in every mode, `EXPLAIN (COSTS OFF)`, beside its
results.

## The run

A run is one mode over the queries asked, under a deadline of five
minutes from its start (`--deadline 300`), as `docs/measuring.md` asks:
the build and install of Tessera, each statement that makes the tables,
each plan and each try count towards it, so run `check` or `setup`
first; the start of the server waits as `pg_ctl` does. Tables whose
`ANALYZE` the deadline stopped are made anew by the next command, as are
tables made from other files or other scripts. Each query is
tried three times (`--tries`), each try on a new connection, as
ClickBench's PostgreSQL entries run a new `psql` for each, and timed
from sending the query to its last row. A try's `statement_timeout` is
`--timeout`, 60 s by default, and at most what is left of the deadline:
a query that does not finish is reported as `timeout`, and a try the
deadline leaves no time for as `deadline`, both untimed. Nothing is
retried. The plans of the queries are written before the timing.

The hot time of a query is the least of its tries after the first, as
ClickBench reads it, and a run's mean is ClickBench's: the geometric
mean of the hot times plus 10 ms. `report` with several runs gives each
query's hot time in each and its ratio to the first run's, both plus
10 ms, and the means over the queries every run timed; below 1 is
faster than the first run.

## A run's directory

`target/bench-runs/clickbench-<id>/`, which git ignores:

- `source.txt`: the commit, the settings, the files and their sizes,
  the hashes of the installed libraries, the power source and the load;
- `pg_settings.txt`: every setting of the server;
- `queries.sql`, `create.sql` and `view.sql`, as they were run;
- `results.txt` and `plans-<mode>.txt` of a check, `times.csv`,
  `plans.txt` and `summary.txt` of a run;
- `run.json`: all of it, which `report` reads.

A full run that a change keeps goes to `docs/benchmarks/`, as
`docs/measuring.md` says.
