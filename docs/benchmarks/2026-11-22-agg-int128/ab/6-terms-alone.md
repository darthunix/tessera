# A/B: a494aaf302b780dfbba4c33841dd3119941b2440 against 624937e9bffa3309c2a8968006cdf5fce2382e73

PostgreSQL 20 (/Users/darthunix/git/postgres-review/build-release/bin/pg_config); instructions counted by pg_pmu; 1 round(s) a side, base and candidate alternating; cases ^g_(group_int2|group_sum|group_sum_numeric|spread_digits18|sum_numeric|runs_digits18|sum_digits18|group_few|group_many)$; repetitions 3; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's count of instructions over the base's; below 1 is less. `min` compares the least count of instructions of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 1% by its minimum and by its median beyond the control.

`disk` compares the kB the plan's nodes report written to disk (Disk Usage, which the core's sort and hash join do not print), where either side wrote any, and is flagged past the same threshold where every count of a side wrote the same; with parallel workers it varies and is not judged. A case is counted fairly only where all its counts on both sides ran one plan shape: EXPLAIN ANALYZE counts the rows of a row-wise node one by one and of a batch node batch by batch. A case whose plans differ has no ratio and is marked `plan differs`, and one whose control's plans differ has no control.

## anyagg

| case | base, M instructions | candidate, M instructions | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| g_group_few | 482.544 | 482.537 | 1.000 | 1.000 | 1.000 |  |
| g_group_int2 | 335.448 | 335.461 | 1.000 | 1.000 | 1.000 |  |
| g_group_many | 475.859 | 475.847 | 1.000 | 1.000 | 1.000 |  |
| g_group_sum | 412.919 | 412.920 | 1.000 | 1.000 | 1.000 |  |
| g_group_sum_numeric | 327.334 | 327.383 | 1.000 | 1.000 | 1.000 |  |
| g_runs_digits18 | 357.089 | 357.102 | 1.000 | 1.000 | 1.000 |  |
| g_spread_digits18 | 412.658 | 412.658 | 1.000 | 1.000 | 1.000 |  |
| g_sum_digits18 | 122.126 | 122.197 | 1.000 | 1.001 | 1.000 |  |
| g_sum_numeric | 85.185 | 85.307 | 1.000 | 1.001 | 1.000 |  |

Runs: base pg-anyagg-m6VAhF; candidate pg-anyagg-sQJwa8.

