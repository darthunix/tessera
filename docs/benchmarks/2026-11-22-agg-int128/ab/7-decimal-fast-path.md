# A/B: WORKTREE@26e42089d9b94739ce9b322771ff387d506827ce against 624937e9bffa3309c2a8968006cdf5fce2382e73

PostgreSQL 20 (/Users/darthunix/git/postgres-review/build-release/bin/pg_config); instructions counted by pg_pmu; 1 round(s) a side, base and candidate alternating; cases ^g_(group_int2|group_sum|group_sum_numeric|spread_digits18|spread_digits19|sum_numeric|runs_digits18|sum_digits18|group_few|group_many)$; repetitions 3; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's count of instructions over the base's; below 1 is less. `min` compares the least count of instructions of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 1% by its minimum and by its median beyond the control.

`disk` compares the kB the plan's nodes report written to disk (Disk Usage, which the core's sort and hash join do not print), where either side wrote any, and is flagged past the same threshold where every count of a side wrote the same; with parallel workers it varies and is not judged. A case is counted fairly only where all its counts on both sides ran one plan shape: EXPLAIN ANALYZE counts the rows of a row-wise node one by one and of a batch node batch by batch. A case whose plans differ has no ratio and is marked `plan differs`, and one whose control's plans differ has no control.

## anyagg

| case | base, M instructions | candidate, M instructions | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| g_group_few | 482.541 | 482.549 | 1.000 | 1.000 | 1.000 |  |
| g_group_int2 | 335.446 | 337.478 | 1.006 | 1.006 | 1.000 |  |
| g_group_many | 475.844 | 475.926 | 1.000 | 1.000 | 1.000 |  |
| g_group_sum | 412.953 | 416.025 | 1.007 | 1.007 | 1.000 |  |
| g_group_sum_numeric | 327.333 | 329.347 | 1.006 | 1.006 | 1.000 |  |
| g_runs_digits18 | 357.103 | 353.288 | 0.989 | 0.989 | 1.000 | faster |
| g_spread_digits18 | 412.680 | 415.882 | 1.008 | 1.008 | 1.000 |  |
| g_spread_digits19 | 1373.230 | 413.059 | 0.301 | 0.301 | 1.000 | faster |
| g_sum_digits18 | 122.129 | 118.216 | 0.968 | 0.968 | 1.000 | faster |
| g_sum_numeric | 85.186 | 86.211 | 1.012 | 1.012 | 1.000 | **slower** |

Runs: base pg-anyagg-eoxBRC; candidate pg-anyagg-1Divd9.

A count does not move with the code's place: a flagged case runs more instructions. Compare the machine code of the functions it runs (tessera-bench --disasm --module nodes --function NAME); a change that trades instructions for memory is judged by time.

