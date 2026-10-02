# Run of the queries derived from TPC-H, 2026-10-02

The 22 queries of [`bench/tpch`](../../../bench/tpch/README.md) at scale
factor 1, by one command, `cargo tpch`, on the cluster the tool keeps:
the answers checked, the plans recorded, both modes timed. The
[README](../../../README.md) cites the range of these speedups.

- **Tessera** at 1f6a7e7 (plan 8.10, the join costed by its own time),
  release build.
- **PostgreSQL** master at 311df1dc (20devel), built without assertions;
  `shared_buffers` 2 GB, `work_mem` 256 MB, `jit` off, no parallel
  workers.
- **Machine**: Apple M5 Pro (6 performance and 12 efficiency cores),
  64 GB, macOS 26.6.2, on AC power, otherwise idle.
- **Data**: SF 1 of `tpchgen` 3.0 (6 001 215 rows of `lineitem`, 1.5 GB
  with the primary keys), the schema with primary keys only, every table
  frozen and in shared buffers (`pg_prewarm`, 100 % by `pg_buffercache`).
- **Method**: two connections, one with `tessera.enable` on and one off,
  each query prepared once in each; with Tessera off the answer must
  match the published one by the precision rules of the specification,
  with it on the answer with it off value for value; then the queries
  that answered the same, one warm-up and 11 pairs of one execution per
  mode, Tessera first in the even pairs and the core first in the odd
  (ABBA), the time taken on the client from sending the query to its
  last row. The table shows medians; the ratio is Tessera's over the
  core's, below one Tessera is faster, with the 95 % bootstrap interval
  over the pairs. "Rows via Tessera" is the share of the rows read from
  the tables that `TessHeapScan` read.
- **Q17 and Q20** run past the time limit of 30 s in both modes: their
  correlated subqueries read `lineitem` once per outer row, and the
  schema has no index on `l_partkey` (`--schema indexed` gives them one).

| query | answer | Tessera nodes | rows via Tessera | off, ms | on, ms | on/off | 95 % interval |
|---|---|--:|--:|--:|--:|--:|--:|
| Q01 pricing summary report | same | 4 | 100 % | 1617.3 | 853.8 | 0.528 | 0.521–0.535 |
| Q02 minimum cost supplier | same | 12 | 97 % | 59.0 | 22.0 | 0.372 | 0.365–0.376 |
| Q03 shipping priority | same | 11 | 100 % | 287.0 | 152.7 | 0.532 | 0.527–0.539 |
| Q04 order priority checking | same | 2 | 93 % | 84.7 | 67.1 | 0.792 | 0.782–0.807 |
| Q05 local supplier volume | same | 14 | 90 % | 109.2 | 86.0 | 0.788 | 0.773–0.802 |
| Q06 forecasting revenue change | same | 3 | 100 % | 208.6 | 152.6 | 0.732 | 0.696–0.745 |
| Q07 volume shipping | same | 10 | 76 % | 174.8 | 137.0 | 0.784 | 0.772–0.794 |
| Q08 national market share | same | 9 | 99 % | 250.3 | 124.4 | 0.497 | 0.490–0.516 |
| Q09 product type profit measure | same | 7 | 92 % | 914.9 | 534.2 | 0.584 | 0.575–0.592 |
| Q10 returned item reporting | same | 10 | 100 % | 327.7 | 226.5 | 0.691 | 0.682–0.697 |
| Q11 important stock identification | same | 15 | 100 % | 48.9 | 24.4 | 0.499 | 0.492–0.504 |
| Q12 shipping modes and order priority | same | 4 | 100 % | 458.2 | 321.9 | 0.702 | 0.643–0.712 |
| Q13 customer distribution | same | 8 | 100 % | 311.5 | 137.4 | 0.441 | 0.430–0.454 |
| Q14 promotion effect | same | 5 | 100 % | 206.4 | 155.8 | 0.755 | 0.749–0.763 |
| Q15 top supplier | same | 7 | 100 % | 212.6 | 145.9 | 0.686 | 0.660–0.702 |
| Q16 parts/supplier relationship | same | 9 | 100 % | 123.4 | 43.1 | 0.349 | 0.344–0.354 |
| Q17 small-quantity-order revenue | TIMEOUT | - | - | - | - | - | - |
| Q18 large volume customer | same | 10 | 100 % | 933.0 | 288.1 | 0.309 | 0.303–0.310 |
| Q19 discounted revenue | same | 6 | 100 % | 367.2 | 321.6 | 0.876 | 0.871–0.883 |
| Q20 potential part promotion | TIMEOUT | - | - | - | - | - | - |
| Q21 suppliers who kept orders waiting | same | 10 | 95 % | 378.7 | 325.5 | 0.859 | 0.840–0.866 |
| Q22 global sales opportunity | same | 6 | 100 % | 85.8 | 73.2 | 0.853 | 0.838–0.869 |

- answers: 20 of 22 the same as the published ones and in both modes; out of time: Q17, Q20
- geometric mean of on/off: 0.604 over 20 queries, 0.604 over the 20 with Tessera nodes
- sum of the medians: off 7.16 s, on 4.19 s
- beyond 3 %: 20 faster, 0 slower; 0 even

The files of the run: [`summary.md`](summary.md) as the tool wrote it,
[`results.txt`](results.txt) (the check of every answer with the one
execution's time in each mode), [`participation.txt`](participation.txt)
(per query and mode: rows read, rows through `TessHeapScan`, rows the
batch filters removed, the Tessera nodes) and [`source.txt`](source.txt)
(commit, settings, SHA-256 of the modules and of `postgres`, power, load).
The plans of both modes of every query (`plans/qNN-on.json`,
`qNN-off.json`) and every timed execution (`timings.csv`) are in the run's
directory, which repeating the run gives.

Derived from TPC-H: the queries and data of the TPC Benchmark H, run with
fixed validation parameters on data from `tpchgen` rather than DBGen,
without the refresh functions and the throughput test. The results are
not comparable to published TPC-H results.
