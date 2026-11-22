# ClickBench stage 2 over Parquet: protocol

Fixed before the runs, 2026-11-22. Task 5.3 of the OpenSpec change
parquet-fdw: the 17 queries that read no string, after the clauses on
integers, dates and times (5.1) and the groups skipped by the footer
(5.2). The maintainer asked for the run the same day.

The queries, by ClickBench's numbers: Q1-Q5, Q7-Q10, Q16, Q20, Q30,
Q33, Q36, Q41-Q43 (every query of queries.sql whose columns hold no
string).

## Questions and hypotheses

1. Does Tessera on beat Tessera off on each of the 17, at 0 and 8
   workers, over the ten files and over hits.parquet? The design's
   criterion of the stage: yes on every query. Expected risks: the
   groupings of many groups (Q16 by UserID, Q33 by WatchID and
   ClientIP, Q36 by ClientIP), whose hash tables grow past work_mem;
   the distinct counts (Q5, Q9, Q10), which stage 1 saw not scale with
   workers (Q5); the bigint aggregates (Q3, Q4), still through the
   generic numeric accumulation.
2. Do the groups skipped by the footer show? Expected: Q41 to Q43
   (CounterID = 62 and a range of EventDate) skip most of the 226 groups
   of hits.parquet, which ClickBench sorts by CounterID and EventDate,
   and win most against Tessera off; the other queries skip nothing.
3. Does the source's check of a clause on stored values show? Expected:
   Q2, Q8 (AdvEngineID <> 0) and Q20 (UserID = constant) beat Tessera
   off by more than the queries without a clause, since no value is
   made for a row the clause rejects.
4. Where does Tessera stand against pg_duckdb, by hot time at the same
   parallelism (DuckDB threads = workers + 1) and by instructions and
   cycles? Recorded as the stage's geometric mean. Expected: DuckDB
   faster overall, as in stage 1; nearest on Q1 and on Q41 to Q43,
   where DuckDB skips the same groups by the same statistics.

## Setup

- Tessera at the branch parquet-fdw head (7d3fdf4c), release build,
  installed by cargo clickbench into PostgreSQL 19rc2
  (~/git/tessera-duckdb/target/pg19), with pg_duckdb 1.2.0 and pg_pmu.
- Cluster of the tool, port 5439, shared_buffers 2 GB, work_mem 64 MB,
  jit off, C locale.
- Files: the ten files of ClickBench's athena_partitioned set (10 M
  rows), then hits.parquet (100 M rows).
- Machine: Apple M5 Pro, 18 cores, 64 GB; the maintainer said to run.
  The runs start only once no other session's Kani (cbmc) runs; power
  recorded by each run.

## Runs, in this order

1. Check: each query alone, over each file set, at 0 and 8 workers:
   the answers of the three modes before any timing.
2. Times: for each file set, the modes off, on, duckdb, each at 0 and
   8 workers, each query a run of its own (three tries on new
   connections, hot time = the least of tries 2 and 3), statement
   timeout 90 s a try, the tool's deadline of 300 s a run. The order
   keeps the tables of a mode: the tool remakes them when it moves
   between duckdb and the foreign table.
3. Counts: over hits.parquet at 0 workers, the modes on and duckdb,
   each query a run of its own, run --counters --tries 5.
4. Reports: cargo clickbench report over the runs of each group.

Nothing is rerun because of its result. A run that fails or times out
is kept and explained.

## Departures, as they happened

- 13:49: the run stopped after the check of Q8 at 0 workers over
  hits.parquet, since another session's Kani started again; resumed by
  a script that waits, before each timed or counted run, until no Kani
  has run for a minute (waits.txt: one wait, of 1040 s, before the first
  run of Q30).
- 15:23: that wait slept a minute before every run, Kani or not, which
  cost time and nothing else; the script was stopped between runs and
  waits only after Kani ran since.
- 15:27: by the maintainer's word, hits.parquet is timed with Tessera on
  and pg_duckdb alone: Tessera off there has Q1 to Q5 and Q7 at 0
  workers, the runs made before the word. The run in progress was cut
  and made again.
- The check of Q33 over hits.parquet compared pg_duckdb's answer with no
  result: its companion query, which checks DuckDB's rows against the
  reference, passed the check's timeout of 300 s. Tessera's answer was
  checked.
- Q30 of Tessera on was counted in three readings of five: the two
  others passed the run's timeout of 60 s.
