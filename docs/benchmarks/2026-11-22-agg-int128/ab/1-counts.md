# A/B: WORKTREE@493ad71556822fcc61caa998a627c845fb5600e1 against 624937e9bffa3309c2a8968006cdf5fce2382e73

PostgreSQL 20 (/Users/darthunix/git/postgres-review/build-release/bin/pg_config); instructions counted by pg_pmu; 1 round(s) a side, base and candidate alternating; cases all; repetitions 3; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's count of instructions over the base's; below 1 is less. `min` compares the least count of instructions of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 1% by its minimum and by its median beyond the control.

`disk` compares the kB the plan's nodes report written to disk (Disk Usage, which the core's sort and hash join do not print), where either side wrote any, and is flagged past the same threshold where every count of a side wrote the same; with parallel workers it varies and is not judged. A case is counted fairly only where all its counts on both sides ran one plan shape: EXPLAIN ANALYZE counts the rows of a row-wise node one by one and of a batch node batch by batch. A case whose plans differ has no ratio and is marked `plan differs`, and one whose control's plans differ has no control.

## anyagg

| case | base, M instructions | candidate, M instructions | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| g_avg_digits18 | 122.149 | 117.224 | 0.960 | 0.960 | 1.000 | faster |
| g_avg_digits19 | 1094.996 | 117.221 | 0.107 | 0.107 | 1.000 | faster |
| g_avg_float8 | 115.683 | 115.685 | 1.000 | 1.000 | 1.000 |  |
| g_avg_int8 | 157.123 | 154.669 | 0.984 | 0.984 | 1.000 | faster |
| g_bit_or | 379.726 | 379.716 | 1.000 | 1.000 | 1.000 |  |
| g_filter | 211.113 | 211.121 | 1.000 | 1.000 | 1.000 |  |
| g_group_few | 482.553 | 482.546 | 1.000 | 1.000 | 1.000 |  |
| g_group_int2 | 335.464 | 338.782 | 1.010 | 1.010 | 1.000 |  |
| g_group_many | 475.854 | 475.849 | 1.000 | 1.000 | 1.000 |  |
| g_group_numeric | 323.893 | 323.919 | 1.000 | 1.000 | 1.000 |  |
| g_group_numeric_col | 1320.697 | 1320.862 | 1.000 | 1.000 | 1.000 |  |
| g_group_sum | 412.931 | 418.595 | 1.014 | 1.014 | 1.000 | **slower** |
| g_group_sum_numeric | 327.328 | 331.804 | 1.014 | 1.014 | 1.000 | **slower** |
| g_max_numeric | 83.628 | 83.627 | 1.000 | 1.000 | 1.000 |  |
| g_max_numeric_col | 621.361 | 621.428 | 1.000 | 1.000 | 1.000 |  |
| g_max_text | 278.944 | 278.958 | 1.000 | 1.000 | 1.000 |  |
| g_runs_digits18 | 357.091 | 352.744 | 0.988 | 0.988 | 1.000 | faster |
| g_runs_digits19 | 1369.018 | 352.745 | 0.258 | 0.258 | 1.000 | faster |
| g_spread_digits18 | 412.650 | 418.324 | 1.014 | 1.014 | 1.000 | **slower** |
| g_spread_digits19 | 1373.219 | 416.248 | 0.303 | 0.303 | 1.000 | faster |
| g_sum_digits18 | 122.132 | 117.220 | 0.960 | 0.960 | 1.000 | faster |
| g_sum_digits19 | 1095.007 | 117.234 | 0.107 | 0.107 | 1.000 | faster |
| g_sum_int8 | 122.117 | 119.664 | 0.980 | 0.980 | 1.000 | faster |
| g_sum_numeric | 85.180 | 85.738 | 1.007 | 1.007 | 1.000 |  |
| g_three | 463.960 | 459.037 | 0.989 | 0.989 | 1.000 | faster |

Runs: base pg-anyagg-4hD0ff; candidate pg-anyagg-C7cnyQ.

A count does not move with the code's place: a flagged case runs more instructions. Compare the machine code of the functions it runs (tessera-bench --disasm --module nodes --function NAME); a change that trades instructions for memory is judged by time.

