# ClickBench over Parquet, stage 1, 2026-11-22

ClickBench's Q1 to Q7 over its Parquet files, read in place by the
wrapper `tessera_parquet`, with Tessera on, Tessera off and pg_duckdb,
at 0, 2, 4 and 8 workers, by `cargo clickbench`
([bench/clickbench](../../../bench/clickbench/README.md)); then the
cycles and instructions of Tessera and DuckDB, which pg_pmu counts.
Stage 1 of the measurement plan of the OpenSpec change `parquet-fdw`:
parallel groups of rows, before any clause is checked at the source.
The questions and hypotheses were fixed before the runs in
[protocol.md](protocol.md).

- **Tessera** at `256e0df5`, release build; **PostgreSQL** 19rc2,
  release, no LLVM; **pg_duckdb** 1.2.0; **pg_pmu**; hashes in
  [source.txt](source.txt).
- **Data**: the ten files `hits_0, 10, ..., 90` of ClickBench's
  athena_partitioned set (10 M rows, 20 groups of rows), and ClickBench's
  `hits.parquet` (100 M rows, 14.8 GB, 226 groups of rows), in the page
  cache. Every answer of the three modes the same at 0 and 8 workers
  (`cargo clickbench check`).
- **Method**: a run is one mode at one count of workers, three tries a
  query on new connections; the hot time is the least of tries 2 and 3.
  pg_duckdb runs with DuckDB threads = workers + 1, as many as the
  processes of PostgreSQL's parallel plan. Counts: five readings a query
  after a warm-up, medians. Every run's id is in [runs.txt](runs.txt),
  its reports in [times.txt](times.txt) and [counts.txt](counts.txt).
- **Machine**: Apple M5 Pro, 18 cores, 64 GB, on AC power, idle but for
  other clusters at rest.

## Times over `hits.parquet`

Hot times in seconds; on/off is Tessera on over Tessera off at the same
workers, below 1 faster.

| query | what | off 0 | on 0 | on 2 | on 4 | on 8 | on/off 8 | duckdb t9 |
|---|---|--:|--:|--:|--:|--:|--:|--:|
| Q1 | `count(*)` | 5.44 | 0.083 | 0.039 | 0.030 | 0.024 | 0.03 | 0.071 |
| Q2 | count where `AdvEngineID <> 0` | 5.67 | 1.04 | 0.39 | 0.25 | 0.19 | 0.23 | 0.085 |
| Q3 | sum, count, avg of two columns | 7.25 | 2.28 | 0.82 | 0.52 | 0.39 | 0.37 | 0.097 |
| Q4 | avg of `UserID` | 6.11 | 3.58 | 1.25 | 0.85 | 0.61 | 0.66 | 0.147 |
| Q5 | count distinct `UserID` | 11.80 | 3.04 | 3.09 | 3.14 | 3.15 | 0.53 | 0.232 |
| Q6 | count distinct `SearchPhrase` | 17.75 | 8.99 | 9.07 | 9.23 | 9.35 | 1.11 | 0.361 |
| Q7 | min, max of `EventDate` | 6.24 | 1.68 | 0.59 | 0.38 | 0.27 | 0.28 | 0.097 |
| mean | geometric, plus 10 ms | 7.83 | 1.68 | 0.86 | 0.66 | 0.54 | 0.33 | 0.144 |

Over the ten files the picture is the same at a tenth of the times;
there Tessera on beats off on every query at every count of workers.

## Counts over `hits.parquet`, serial

Medians in billions; DuckDB with one thread. With 8 workers and 9
threads the sums grow by 7 % (Tessera's cycles) and 9 % (DuckDB's).

| query | Tessera cycles | DuckDB cycles | ratio | Tessera instructions | DuckDB instructions | ratio |
|---|--:|--:|--:|--:|--:|--:|
| Q1 | 0.34 | 0.27 | 0.78 | 1.78 | 1.24 | 0.70 |
| Q2 | 4.53 | 0.43 | 0.09 | 29.3 | 2.42 | 0.08 |
| Q3 | 9.81 | 1.01 | 0.10 | 63.1 | 4.22 | 0.07 |
| Q4 | 16.37 | 2.98 | 0.18 | 107.5 | 7.85 | 0.07 |
| Q5 | 13.22 | 6.07 | 0.46 | 46.8 | 10.8 | 0.23 |
| Q6 | 39.84 | 8.87 | 0.22 | 169.4 | 24.9 | 0.15 |
| Q7 | 6.99 | 0.88 | 0.13 | 51.3 | 5.72 | 0.11 |
| sum | 91.1 | 20.5 | 0.23 | 469 | 57.2 | 0.12 |

## Findings

1. **Tessera on beats off on every query but one.** Q6 at 8 workers
   over `hits.parquet` takes 9.35 s with Tessera on and 8.42 s off. Q5
   and Q6 are `COUNT(DISTINCT)`: Tessera plans `TessAgg` over a serial
   `TessParquetScan`, since its distinct aggregate has no parallel
   form, and the costs choose it over the core's parallel plan, seven
   workers sorting their share under a Gather Merge, which at 8 workers
   is the faster on Q6. So Q5 and Q6 do not scale with workers; the
   stage's criterion is not met on Q6 at 8 workers.
2. **The other five scale with the workers on the one file**: from 0 to
   8 workers Q2 by 5.6×, Q3 by 5.8×, Q4 by 5.9×, Q7 by 6.2×, over 226
   groups of rows.
3. **DuckDB is 3.7× faster by the mean at 8 workers**, 2.2× on Q2 to
   26× on Q6, and Tessera faster on Q1 alone from 2 workers on. By
   counts, serially, DuckDB does the same work with 12 % of Tessera's
   instructions and 23 % of its cycles: Tessera spends about 290
   instructions a row on Q2, DuckDB 24. Tessera's instructions run at a
   higher rate (6.5 a cycle on Q2 against DuckDB's 5.6), so its cost is
   the work it does, not stalls: making a value of every row it reads
   before any clause is checked, which stage 2, the clauses checked on
   stored values, is to cut.
4. **The source's share of a row's cost**, a quarter of the wrapper's,
   which the plan left to this measurement: the times of the serial
   scans of one or two columns with and without Tessera give 0.18 (Q2),
   0.31 (Q3) and 0.27 (Q7), so the quarter stays.
