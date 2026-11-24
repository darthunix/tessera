# A/B: WORKTREE@cb7c230a6325128b52d7da4acd45948eb24c3ade against 624937e9bffa3309c2a8968006cdf5fce2382e73

PostgreSQL 20 (/Users/darthunix/git/postgres-review/build-release/bin/pg_config); instructions counted by pg_pmu; 1 round(s) a side, base and candidate alternating; cases all; repetitions 3; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's count of instructions over the base's; below 1 is less. `min` compares the least count of instructions of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 1% by its minimum and by its median beyond the control.

`disk` compares the kB the plan's nodes report written to disk (Disk Usage, which the core's sort and hash join do not print), where either side wrote any, and is flagged past the same threshold where every count of a side wrote the same; with parallel workers it varies and is not judged. A case is counted fairly only where all its counts on both sides ran one plan shape: EXPLAIN ANALYZE counts the rows of a row-wise node one by one and of a batch node batch by batch. A case whose plans differ has no ratio and is marked `plan differs`, and one whose control's plans differ has no control.

## anyagg

| case | base, M instructions | candidate, M instructions | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| g_avg_digits18 | 122.127 | 117.254 | 0.960 | 0.960 | 1.000 | faster |
| g_avg_digits19 | 1094.986 | 117.254 | 0.107 | 0.107 | 1.000 | faster |
| g_avg_float8 | 115.682 | 115.684 | 1.000 | 1.000 | 1.000 |  |
| g_avg_int8 | 157.122 | 154.678 | 0.984 | 0.984 | 1.000 | faster |
| g_bit_or | 379.705 | 379.722 | 1.000 | 1.000 | 1.000 |  |
| g_filter | 211.115 | 211.114 | 1.000 | 1.000 | 1.000 |  |
| g_group_few | 482.554 | 482.544 | 1.000 | 1.000 | 1.000 |  |
| g_group_int2 | 335.452 | 337.479 | 1.006 | 1.006 | 1.000 |  |
| g_group_many | 475.847 | 475.858 | 1.000 | 1.000 | 1.000 |  |
| g_group_numeric | 323.889 | 323.901 | 1.000 | 1.000 | 1.000 |  |
| g_group_numeric_col | 1320.731 | 1320.781 | 1.000 | 1.000 | 1.000 |  |
| g_group_sum | 412.932 | 416.049 | 1.008 | 1.008 | 1.000 |  |
| g_group_sum_numeric | 327.333 | 329.347 | 1.006 | 1.006 | 1.000 |  |
| g_max_numeric | 83.627 | 83.628 | 1.000 | 1.000 | 1.000 |  |
| g_max_numeric_col | 621.381 | 621.363 | 1.000 | 1.000 | 1.000 |  |
| g_max_text | 278.939 | 278.947 | 1.000 | 1.000 | 1.000 |  |
| g_runs_digits18 | 357.108 | 353.210 | 0.989 | 0.989 | 1.000 | faster |
| g_runs_digits19 | 1368.974 | 353.200 | 0.258 | 0.258 | 1.000 | faster |
| g_spread_digits18 | 412.664 | 415.864 | 1.008 | 1.008 | 1.000 |  |
| g_spread_digits19 | 1373.219 | 413.099 | 0.301 | 0.301 | 1.000 | faster |
| g_sum_digits18 | 122.133 | 117.255 | 0.960 | 0.960 | 1.000 | faster |
| g_sum_digits19 | 1094.981 | 117.241 | 0.107 | 0.107 | 1.000 | faster |
| g_sum_int8 | 122.115 | 119.683 | 0.980 | 0.980 | 1.000 | faster |
| g_sum_numeric | 85.184 | 85.244 | 1.001 | 1.001 | 1.000 |  |
| g_three | 463.956 | 459.053 | 0.989 | 0.989 | 1.000 | faster |

Runs: base pg-anyagg-SZ6ict; candidate pg-anyagg-9obI9B.

