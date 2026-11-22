# ClickBench stage 1 over Parquet: protocol

Fixed before the runs, 2026-11-22. Task 4.6 of the OpenSpec change
parquet-fdw, and the maintainer's ask the same day: after the run,
count the instructions and cycles of Tessera and DuckDB with pg_pmu.

## Questions and hypotheses

1. Does Tessera on beat Tessera off on each of Q1 to Q7, at 0, 2, 4
   and 8 workers, over the ten files and over `hits.parquet`? The
   design's criterion of stage 1: yes on every query.
2. Does Tessera on scale with the workers on the one file? Expected:
   the time falls with 2, 4 and 8 workers on `hits.parquet`, as long as
   the file has more groups of rows than participants (the participants
   take whole groups).
3. Where does Tessera stand against pg_duckdb over the same files, by
   hot time at the same parallelism (DuckDB threads = workers + 1), and
   by cycles and instructions? Expected from the check's single tries:
   DuckDB faster on every query, most on Q2 to Q6, the aggregates over
   one or two columns, where DuckDB reads its stored values in vectors;
   Q1 (`count(*)`) near even, both answering from the footers.

## Setup

- Tessera at the branch parquet-fdw head (256e0df5), release build,
  installed by `cargo clickbench` into PostgreSQL 19rc2
  (`~/git/tessera-duckdb/target/pg19`, release, no LLVM), with
  pg_duckdb 1.2.0 and pg_pmu.
- Cluster of the tool, port 5439, shared_buffers 2 GB, work_mem 64 MB,
  jit off, C locale.
- Files: the ten files of ClickBench's athena_partitioned set
  (`hits_0, 10, ..., 90`, 10 M rows), then `hits.parquet` (100 M rows),
  in `~/git/tessera-duckdb/target/clickbench-data`.
- Machine: Apple M5 Pro, 18 cores, 64 GB; the maintainer said it is
  free; power recorded by each run. Other idle clusters of other
  worktrees stay up (ports 5433, 5435, 5436, 55476, 7400-7404).

## Runs, in this order

1. `check --queries 1-7` over each file set at workers 0 and 8: the
   answers of the three modes before any timing.
2. Times: for each file set, for workers 0, 2, 4, 8, the modes off, on,
   duckdb (threads workers + 1): `run --queries 1-7`, three tries a
   query on new connections, hot time = the least of tries 2 and 3.
   Each run under the tool's deadline of 300 s.
3. Counts: for each file set, for workers 0 and 8, the modes on and
   duckdb: `run --counters --queries 1-7 --tries 5`, medians of five
   readings after a warm-up.
4. Reports: `cargo clickbench report` over the runs of each group.

Nothing is rerun because of its result. A run that fails or times out
is kept and explained.
