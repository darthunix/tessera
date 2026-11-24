# A/B: WORKTREE@26e42089d9b94739ce9b322771ff387d506827ce against 624937e9bffa3309c2a8968006cdf5fce2382e73

PostgreSQL 20 (/Users/darthunix/git/postgres-review/build-release/bin/pg_config); instructions counted by pg_pmu; 1 round(s) a side, base and candidate alternating; cases all; repetitions 3; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's count of instructions over the base's; below 1 is less. `min` compares the least count of instructions of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 1% by its minimum and by its median beyond the control.

`disk` compares the kB the plan's nodes report written to disk (Disk Usage, which the core's sort and hash join do not print), where either side wrote any, and is flagged past the same threshold where every count of a side wrote the same; with parallel workers it varies and is not judged. A case is counted fairly only where all its counts on both sides ran one plan shape: EXPLAIN ANALYZE counts the rows of a row-wise node one by one and of a batch node batch by batch. A case whose plans differ has no ratio and is marked `plan differs`, and one whose control's plans differ has no control.

## anyagg

| case | base, M instructions | candidate, M instructions | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| g_avg_digits18 | 122.141 | 118.222 | 0.968 | 0.968 | 1.000 | faster |
| g_avg_digits19 | 1094.997 | 118.221 | 0.108 | 0.108 | 1.000 | faster |
| g_avg_float8 | 115.691 | 115.683 | 1.000 | 1.000 | 1.000 |  |
| g_avg_int8 | 157.113 | 155.148 | 0.987 | 0.987 | 1.000 | faster |
| g_bit_or | 379.715 | 379.746 | 1.000 | 1.000 | 1.000 |  |
| g_filter | 211.117 | 211.116 | 1.000 | 1.000 | 1.000 |  |
| g_group_few | 482.560 | 482.543 | 1.000 | 1.000 | 1.000 |  |
| g_group_int2 | 335.461 | 339.845 | 1.013 | 1.013 | 1.000 | **slower** |
| g_group_many | 475.852 | 475.880 | 1.000 | 1.000 | 1.000 |  |
| g_group_numeric | 323.960 | 323.916 | 1.000 | 1.000 | 1.000 |  |
| g_group_numeric_col | 1320.770 | 1320.976 | 1.000 | 1.000 | 1.000 |  |
| g_group_sum | 412.946 | 418.852 | 1.014 | 1.014 | 1.000 | **slower** |
| g_group_sum_numeric | 327.330 | 331.684 | 1.013 | 1.013 | 1.000 | **slower** |
| g_max_numeric | 83.636 | 83.634 | 1.000 | 1.000 | 1.000 |  |
| g_max_numeric_col | 621.374 | 621.368 | 1.000 | 1.000 | 1.000 |  |
| g_max_text | 278.937 | 278.940 | 1.000 | 1.000 | 1.000 |  |
| g_runs_digits18 | 357.114 | 353.402 | 0.990 | 0.990 | 1.000 | faster |
| g_runs_digits19 | 1368.973 | 353.425 | 0.258 | 0.258 | 1.000 | faster |
| g_spread_digits18 | 412.671 | 420.471 | 1.019 | 1.019 | 1.000 | **slower** |
| g_spread_digits19 | 1373.199 | 419.057 | 0.305 | 0.305 | 1.000 | faster |
| g_sum_digits18 | 122.132 | 118.219 | 0.968 | 0.968 | 1.000 | faster |
| g_sum_digits19 | 1095.006 | 118.222 | 0.108 | 0.108 | 1.000 | faster |
| g_sum_int8 | 122.116 | 120.164 | 0.984 | 0.984 | 1.000 | faster |
| g_sum_numeric | 85.184 | 86.210 | 1.012 | 1.012 | 1.000 | **slower** |
| g_three | 463.956 | 460.022 | 0.991 | 0.992 | 1.000 |  |

Runs: base pg-anyagg-ZRvyp0; candidate pg-anyagg-Np8KUE.

A count does not move with the code's place: a flagged case runs more instructions. Compare the machine code of the functions it runs (tessera-bench --disasm --module nodes --function NAME); a change that trades instructions for memory is judged by time.

