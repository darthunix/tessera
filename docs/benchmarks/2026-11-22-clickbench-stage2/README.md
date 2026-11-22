# ClickBench over Parquet, stage 2, 2026-11-22

ClickBench's 17 queries that read no string column (Q1 to Q5, Q7 to
Q10, Q16, Q20, Q30, Q33, Q36, Q41 to Q43), over its Parquet files read
in place by the wrapper `tessera_parquet`, by `cargo clickbench`
([bench/clickbench](../../../bench/clickbench/README.md)): Tessera on
against pg_duckdb, by time and by the cycles and instructions pg_pmu
counts; over the ten files, Tessera off too. Stage 2 of the measurement
plan of the OpenSpec change `parquet-fdw`, after the clauses checked on
stored values and the groups of rows skipped by the footer. The
questions and hypotheses were fixed before the runs in
[protocol.md](protocol.md), which lists the departures from it.

- **Tessera** at `7d3fdf4c`, release build; **PostgreSQL** 19rc2,
  release, no LLVM; **pg_duckdb** 1.2.0; **pg_pmu**; hashes in
  [source.txt](source.txt). The branch did not hold the change
  `agg-int128`, merged into `main` the same day: Q4's `avg(UserID)`,
  whose values have 19 digits, still summed them in `numeric` a row.
- **Data**: the ten files `hits_0, 10, ..., 90` of ClickBench's
  athena_partitioned set (10 M rows), and `hits.parquet` (100 M rows,
  14.8 GB, 226 groups of rows), in the page cache. Every answer of
  Tessera on equal to Tessera off's at 0 and 8 workers over both
  (`cargo clickbench check`); pg_duckdb's too, but Q33 over
  `hits.parquet`, whose comparison passed its timeout.
- **Method**: a run is one query in one mode at one count of workers,
  three tries on new connections; the hot time is the least of tries 2
  and 3. pg_duckdb runs with DuckDB threads = workers + 1. Counts: five
  readings a query after a warm-up, medians. The runs' ids are in
  [runs.txt](runs.txt), their reports in [times.txt](times.txt) and
  [counts.txt](counts.txt), made by [report.sh](report.sh); the plans
  the findings cite in [plans.txt](plans.txt). The runs' raw data stay
  in `target/bench-runs/` of the machine.
- **Machine**: Apple M5 Pro, 18 cores, 64 GB, on AC power. A timed or
  counted run started only when no other session's Kani ran; when one
  was running, a minute after it ended ([waits.txt](waits.txt)).
- **Not measured**, by the maintainer's word during the run: Tessera
  off over `hits.parquet` but for Q1 to Q5 and Q7 at 0 workers.

## Times over `hits.parquet`

Hot times in seconds. duckdb/on is DuckDB's time over Tessera's at the
same parallelism, both plus 10 ms: below 1 DuckDB is faster.

At 0 workers, DuckDB with one thread:

| query | what | on | duckdb | duckdb/on |
|---|---|--:|--:|--:|
| Q1 | `count(*)` | 0.082 | 0.063 | 0.80 |
| Q2 | count where `AdvEngineID <> 0` | 0.159 | 0.122 | 0.78 |
| Q3 | sum, count, avg of two columns | 1.078 | 0.234 | 0.22 |
| Q4 | avg of `UserID` | 2.983 | 0.673 | 0.23 |
| Q5 | count distinct `UserID` | 2.425 | 1.400 | 0.58 |
| Q7 | min, max of `EventDate` | 1.041 | 0.213 | 0.21 |
| Q8 | count by `AdvEngineID <> 0` | 0.381 | 0.127 | 0.35 |
| Q9 | count distinct users by region | 20.25 | 1.727 | 0.09 |
| Q10 | four aggregates by region | 22.57 | 2.515 | 0.11 |
| Q16 | count by `UserID` | 3.332 | 1.577 | 0.47 |
| Q20 | `UserID = constant` | 0.085 | 0.099 | 1.14 |
| Q30 | 90 sums of `ResolutionWidth + k` | 30.14 | 0.211 | 0.01 |
| Q33 | count by `WatchID, ClientIP` | 16.74 | 7.688 | 0.46 |
| Q36 | count by four of `ClientIP` | 4.149 | 1.330 | 0.32 |
| Q41 | `CounterID = 62`, a month, two hashes | 0.036 | 0.076 | 1.89 |
| Q42 | `CounterID = 62`, a month, a hash | 0.038 | 0.076 | 1.79 |
| Q43 | `CounterID = 62`, two days, by minute | 0.032 | 0.083 | 2.24 |
| mean | geometric, plus 10 ms | 1.021 | 0.383 | 0.38 |

At 8 workers, DuckDB with nine threads:

| query | on | duckdb | duckdb/on |
|---|--:|--:|--:|
| Q1 | 0.024 | 0.069 | 2.37 |
| Q2 | 0.039 | 0.083 | 1.91 |
| Q3 | 0.179 | 0.099 | 0.58 |
| Q4 | 0.540 | 0.147 | 0.28 |
| Q5 | 2.437 | 0.230 | 0.10 |
| Q7 | 0.164 | 0.094 | 0.60 |
| Q8 | 0.273 | 0.080 | 0.32 |
| Q9 | 9.180 | 0.281 | 0.03 |
| Q10 | 10.43 | 0.383 | 0.04 |
| Q16 | 2.315 | 0.261 | 0.12 |
| Q20 | 0.030 | 0.092 | 2.54 |
| Q30 | 4.998 | 0.099 | 0.02 |
| Q33 | 17.60 | 1.218 | 0.07 |
| Q36 | 2.394 | 0.254 | 0.11 |
| Q41 | 0.030 | 0.077 | 2.18 |
| Q42 | 0.033 | 0.073 | 1.94 |
| Q43 | 0.045 | 0.079 | 1.63 |
| mean | 0.479 | 0.157 | 0.33 |

Tessera off at 0 workers took 5.3 to 11.5 s on Q1 to Q5 and Q7, 6.4 s
by their mean, against Tessera on's 0.71 s: 9.0 times faster, 1.9
times on Q4, the least.

Against stage 1, at 8 workers over the same file, Tessera on took Q2 in
0.039 s instead of 0.19, Q3 in 0.179 instead of 0.39, Q7 in 0.164
instead of 0.27, Q4 in 0.540 instead of 0.61, Q5 in 2.44 instead of
3.15; Q1 is unchanged.

## Times over the ten files

The means, geometric, plus 10 ms, over the 17 queries; the per-query
times are in [times.txt](times.txt).

| workers | on | off | off/on | duckdb | duckdb/on |
|--:|--:|--:|--:|--:|--:|
| 0 | 0.137 | 1.084 | 7.90 | 0.066 | 0.48 |
| 8 | 0.090 | 0.338 | 3.74 | 0.039 | 0.43 |

Tessera on beats off on every query at 0 workers, by 1.17 times on Q30
and 1.18 on Q10 the least, and on every query at 8 workers but Q10:
2.03 s on, 1.17 s off.

## Counts over `hits.parquet`, serial

Medians in billions, of Tessera on and then of DuckDB with one thread;
a ratio is DuckDB's count over Tessera's. Q30 of Tessera on is counted
from three readings of five, the two others past the run's timeout of
60 s; it is left out of the sums.

| query | cycles | duckdb | ratio | instructions | duckdb | ratio |
|---|--:|--:|--:|--:|--:|--:|
| Q1 | 0.35 | 0.22 | 0.61 | 1.87 | 1.24 | 0.66 |
| Q2 | 0.68 | 0.49 | 0.72 | 3.24 | 2.41 | 0.75 |
| Q3 | 4.61 | 0.94 | 0.20 | 27.8 | 4.22 | 0.15 |
| Q4 | 12.8 | 2.96 | 0.23 | 89.9 | 7.84 | 0.09 |
| Q5 | 10.5 | 6.02 | 0.57 | 29.2 | 10.8 | 0.37 |
| Q7 | 4.62 | 0.85 | 0.18 | 33.7 | 5.72 | 0.17 |
| Q8 | 1.61 | 0.48 | 0.30 | 7.87 | 2.46 | 0.31 |
| Q9 | 83.7 | 7.50 | 0.09 | 469 | 16.4 | 0.03 |
| Q10 | 97.3 | 11.0 | 0.11 | 532 | 24.3 | 0.05 |
| Q16 | 15.0 | 6.88 | 0.46 | 53.2 | 12.5 | 0.23 |
| Q20 | 0.34 | 0.39 | 1.16 | 1.36 | 1.92 | 1.41 |
| Q30 | 138 | 0.86 | 0.01 | 704 | 3.65 | 0.01 |
| Q33 | 71.1 | 33.3 | 0.47 | 420 | 86.1 | 0.20 |
| Q36 | 18.0 | 5.85 | 0.33 | 70.1 | 11.0 | 0.16 |
| Q41 | 0.12 | 0.30 | 2.50 | 0.60 | 1.44 | 2.41 |
| Q42 | 0.14 | 0.26 | 1.95 | 0.69 | 1.43 | 2.07 |
| Q43 | 0.11 | 0.30 | 2.67 | 0.60 | 1.76 | 2.96 |
| sum but Q30 | 321 | 77.8 | 0.24 | 1741 | 192 | 0.11 |

## Findings

1. **The clauses on stored values and the groups skipped work.** Q2,
   `count(*)` where `AdvEngineID <> 0`, spends 32 instructions a row,
   where it spent 293 in stage 1 and DuckDB spends 24. Q20, Q41, Q42
   and Q43, which a clause decides, are where Tessera beats DuckDB, by
   1.1 to 3 times in time and in instructions, and Q1 and Q2 at 8
   workers. Q41 to Q43 take 32 to 38 ms serially over the 14.8 GB
   file, though they read more columns than Q2, which reads one in full
   in 159 ms: they cannot have read most of it. How many groups they
   skipped is not kept: the plans are without ANALYZE.
2. **Against Tessera off, hypothesis 2 of the protocol does not show
   and hypothesis 3 shows in part.** Over the ten files at 0 workers
   Q41 to Q43 gain 24.7 to 28.7 times, Q20 30.2 and Q1 28.2: with
   Tessera on these take 9 to 21 ms, a query's fixed cost, so their
   gains are Tessera off's times. Q2 and Q8 gain 20.0 and 11.3 times,
   more than every query that reads a column without a clause (1.2 to
   7.5) but less than Q1, which reads no column; over `hits.parquet` Q2
   gains 31.5 against 1.9 to 6.0 and Q1's 57.8.
3. **The stage's criterion is not met on Q10 at 8 workers over the ten
   files**, 2.03 s with Tessera on and 1.17 s off. `TessAgg` does not
   take Q10 (finding 4), and the costs choose a serial
   `TessParquetScan` under the core's sort and `GroupAggregate` over the
   core's parallel plan, workers sorting their share under a Gather
   Merge: the finding of stage 1 on Q5 and Q6. Over `hits.parquet`
   Tessera off was not timed there.
4. **Grouped `COUNT(DISTINCT)` goes to the core over `hits.parquet`**
   (Q9, Q10). `TessAgg` groups it, as Q9 over the ten files shows, but
   the table of the pairs of group and value does not spill, so the
   planner gives it the query only when its estimate of the pairs fits
   `hash_mem`; over 100 M rows the pairs of region and user do not.
   The core sorts every row by region and user, 469 and 532 billion
   instructions, 9 to 33 times DuckDB's time. Q10 goes to the core over
   the ten files too: its sum and avg of `smallint` are the node's
   generic aggregates, whose gate counts the groups by the core's
   hashed plans, and the core builds none when an aggregate has
   DISTINCT. Both causes are read from the planner's code and the
   plans, not traced in the planner.
5. **Q30 is the core's aggregate**: `TessAgg` takes at most 64
   aggregates (`AGG_MAX_GROUPED`), and Q30 has 90. The core sums the
   rows a value at a time, 704 billion instructions, 143 times DuckDB's
   time serially.
6. **The largest groupings do not scale with workers**: Q33, 100 M
   groups by `WatchID, ClientIP`, plans a serial `TessAgg` at 8 workers
   and takes 17.6 s, as at 0 workers; DuckDB takes 1.2 s. Q5, the
   distinct count, is serial too, 2.4 s at 0 and 8 workers.
7. **DuckDB is 2.7 times faster by the mean serially and 3 times at 8
   workers**; by counts it does the 16 queries but Q30 with 11 % of
   Tessera's instructions and 24 % of its cycles. Without Q9, Q10 and
   Q30, whose every row the core's aggregate takes, Tessera's
   instructions are 4.9 times DuckDB's on the other 14.
