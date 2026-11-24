# A/B: WORKTREE@56552e1a72b4422227e15fd50d864ad2fe7fa7f7 against 9c76fd3e27d16304e20ddb0ec182c1cb9c6fd2ed

PostgreSQL 19 (/private/tmp/claude-501/-Users-darthunix-git-tessera/b6a5d83c-0cf8-4b12-9221-e9e15b8eaa8d/scratchpad/pg19-count/bin/pg_config); instructions counted by pg_pmu; 1 round(s) a side, base and candidate alternating; cases all; repetitions 3; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's count of instructions over the base's; below 1 is less. `min` compares the least count of instructions of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 1% by its minimum and by its median beyond the control.

`disk` compares the kB the plan's nodes report written to disk (Disk Usage, which the core's sort and hash join do not print), where either side wrote any, and is flagged past the same threshold where every count of a side wrote the same; with parallel workers it varies and is not judged. A case is counted fairly only where all its counts on both sides ran one plan shape: EXPLAIN ANALYZE counts the rows of a row-wise node one by one and of a batch node batch by batch. A case whose plans differ has no ratio and is marked `plan differs`, and one whose control's plans differ has no control.

## anykey

| case | base, M instructions | candidate, M instructions | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| any_distinct | 393.133 | 393.101 | 1.000 | 1.000 | 1.000 |  |
| any_join_numeric | 1982.309 | 1982.321 | 1.000 | 1.000 | 1.000 |  |
| any_join_semi | 1469.243 | 1469.277 | 1.000 | 1.000 | 1.000 |  |
| any_join_text | 1359.443 | 1359.377 | 1.000 | 1.000 | 1.000 |  |
| any_join_two | 1679.660 | 1679.659 | 1.000 | 1.000 | 1.000 |  |
| any_numeric | 456.192 | 456.199 | 1.000 | 1.000 | 1.000 |  |
| any_text_agg | 385.680 | 385.659 | 1.000 | 1.000 | 1.000 |  |
| any_text_col | 485.003 | 484.996 | 1.000 | 1.000 | 1.000 |  |
| any_text_few | 359.295 | 359.311 | 1.000 | 1.000 | 1.000 |  |
| any_union | 758.171 | 758.198 | 1.000 | 1.000 | 1.000 |  |

Runs: base pg-anykey-2g0hlX; candidate pg-anykey-qMfZzC.

## sort

| case | base, M instructions | candidate, M instructions | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| distinct_agg | 489.136 | 489.140 | 1.000 | 1.000 | 1.003 |  |
| distinct_few | 500.924 | 501.036 | 1.000 | 1.000 | 1.000 |  |
| distinct_group | 993.076 | 992.913 | 1.000 | 1.000 | 1.001 |  |
| distinct_many | 798.915 | 799.013 | 1.000 | 1.000 | 1.000 |  |
| order_big | 1210.389 | 1210.166 | 1.000 | 1.000 | 0.986 |  |
| order_desc | 1247.400 | 1247.538 | 1.000 | 1.000 | 0.990 |  |
| order_few | 1103.253 | 1103.458 | 1.000 | 1.000 | 0.987 |  |
| order_few_text | 11120.509 | 11121.230 | 1.000 | 1.000 | 1.001 |  |
| order_filter | 385.747 | 385.832 | 1.000 | 1.000 | 0.995 |  |
| order_int | 1103.613 | 1103.762 | 1.000 | 1.000 | 1.000 |  |
| order_multi | 1427.801 | 1427.940 | 1.000 | 1.000 | 0.995 |  |
| order_numeric | 2511.027 | 2519.444 | 1.003 | 1.003 | 1.000 |  |
| order_out | 1241.968 | 1242.105 | 1.000 | 1.000 | 0.990 |  |
| order_sorted | 757.124 | 757.206 | 1.000 | 1.000 | 0.997 |  |
| order_text_c | 7657.663 | 4587.329 | 0.599 | 0.599 | 0.999 | faster |
| order_text_out | 1663.600 | 1663.953 | 1.000 | 1.000 | 0.991 |  |
| order_wide | 1629.133 | 1629.235 | 1.000 | 1.000 | 0.991 |  |
| topn_few | 410.023 | 410.238 | 1.001 | 1.001 | 1.000 |  |
| topn_many | 721.918 | 721.765 | 1.000 | 1.000 | 0.998 |  |
| topn_numeric | 1631.759 | 1635.775 | 1.002 | 1.002 | 1.000 |  |
| topn_offset | 421.128 | 421.430 | 1.001 | 1.001 | 1.001 |  |
| topn_reverse | 1212.010 | 1212.262 | 1.000 | 1.000 | 1.001 |  |
| topn_text_c | 1347.213 | 1003.885 | 0.745 | 0.745 | 1.000 | faster |

Runs: base pg-sort-gyH07j; candidate pg-sort-smqsnf.

