# A/B: WORKTREE@26e42089d9b94739ce9b322771ff387d506827ce against 624937e9bffa3309c2a8968006cdf5fce2382e73

PostgreSQL 20 (/Users/darthunix/git/postgres-review/build-release/bin/pg_config); instructions counted by pg_pmu; 1 round(s) a side, base and candidate alternating; cases ^g_(sum_numeric|sum_digits18|sum_digits19|avg_digits18|sum_int8|group_sum_numeric|spread_digits18)$; repetitions 3; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's count of instructions over the base's; below 1 is less. `min` compares the least count of instructions of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 1% by its minimum and by its median beyond the control.

`disk` compares the kB the plan's nodes report written to disk (Disk Usage, which the core's sort and hash join do not print), where either side wrote any, and is flagged past the same threshold where every count of a side wrote the same; with parallel workers it varies and is not judged. A case is counted fairly only where all its counts on both sides ran one plan shape: EXPLAIN ANALYZE counts the rows of a row-wise node one by one and of a batch node batch by batch. A case whose plans differ has no ratio and is marked `plan differs`, and one whose control's plans differ has no control.

## anyagg

| case | base, M instructions | candidate, M instructions | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| g_avg_digits18 | 122.134 | 117.258 | 0.960 | 0.960 | 1.000 | faster |
| g_group_sum_numeric | 327.280 | 329.314 | 1.006 | 1.006 | 1.000 |  |
| g_spread_digits18 | 412.559 | 415.779 | 1.008 | 1.008 | 1.000 |  |
| g_sum_digits18 | 122.136 | 117.242 | 0.960 | 0.960 | 1.000 | faster |
| g_sum_digits19 | 1095.050 | 117.253 | 0.107 | 0.107 | 1.000 | faster |
| g_sum_int8 | 122.129 | 119.690 | 0.980 | 0.980 | 1.000 | faster |
| g_sum_numeric | 85.183 | 85.249 | 1.001 | 1.001 | 1.000 |  |

Runs: base pg-anyagg-ldalhx; candidate pg-anyagg-Y7Ta71.

