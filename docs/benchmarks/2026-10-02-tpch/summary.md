# Queries derived from TPC-H, run 0dUc2N

SF 1, schema pk, serial, work_mem 256MB, jit off, 11 pairs ABBA after 1 warm-up; 1f6a7e7204d7 at 2026-10-02 10:55:33.871516+07, PostgreSQL 20devel, AC Power

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

The ratio is the median time with Tessera on over the median with it off: below one Tessera is faster. The interval is the 95 % bootstrap interval of the ratio over the pairs. Rows via Tessera is the share of the rows read from the tables that TessHeapScan read.

Derived from TPC-H: the queries and data of the TPC Benchmark H, run with fixed validation parameters on data from tpchgen rather than DBGen, without the refresh functions and the throughput test. The results are not comparable to published TPC-H results.
