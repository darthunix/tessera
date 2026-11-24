# A/B: WORKTREE@81f23392b5a8e5139082c58cd8e44ebf96b278a3 against f8ee89ef1999c388312d20b874fb65ef76c92d03

PostgreSQL 19 (/private/tmp/claude-501/-Users-darthunix-git-tessera/b6a5d83c-0cf8-4b12-9221-e9e15b8eaa8d/scratchpad/pg19-count/bin/pg_config); instructions counted by pg_pmu; 1 round(s) a side, base and candidate alternating; cases all; repetitions 3; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's count of instructions over the base's; below 1 is less. `min` compares the least count of instructions of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 1% by its minimum and by its median beyond the control.

`disk` compares the kB the plan's nodes report written to disk (Disk Usage, which the core's sort and hash join do not print), where either side wrote any, and is flagged past the same threshold where every count of a side wrote the same; with parallel workers it varies and is not judged. A case is counted fairly only where all its counts on both sides ran one plan shape: EXPLAIN ANALYZE counts the rows of a row-wise node one by one and of a batch node batch by batch. A case whose plans differ has no ratio and is marked `plan differs`, and one whose control's plans differ has no control.

## anykey

| case | base, M instructions | candidate, M instructions | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| any_distinct | 391.358 | 406.982 | 1.040 | 1.040 | 1.000 | **slower** |
| any_join_numeric | 1980.806 | 1982.317 | 1.001 | 1.001 | 1.000 |  |
| any_join_semi | 1469.223 | 1469.221 | 1.000 | 1.000 | 1.000 |  |
| any_join_text | 1352.833 | 1359.380 | 1.005 | 1.005 | 1.000 |  |
| any_join_two | 1673.124 | 1679.648 | 1.004 | 1.004 | 1.000 |  |
| any_numeric | 455.754 | 456.197 | 1.001 | 1.001 | 1.000 |  |
| any_text_agg | 383.572 | 398.711 | 1.039 | 1.039 | 1.000 | **slower** |
| any_text_col | 483.013 | 483.159 | 1.000 | 1.000 | 1.000 |  |
| any_text_few | 357.544 | 372.361 | 1.041 | 1.041 | 1.000 | **slower** |
| any_union | 754.681 | 785.125 | 1.040 | 1.040 | 1.000 | **slower** |

Runs: base pg-anykey-UyRMwH; candidate pg-anykey-mWRg5c.

## rowwise

| case | base, M instructions | candidate, M instructions | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| row_column | 379.705 | 379.942 | 1.001 | 1.001 | 1.000 |  |
| row_expr | 439.635 | 440.643 | 1.002 | 1.002 | 1.000 |  |
| row_first | 0.049 | 0.049 | 1.000 | 1.000 | 1.000 |  |
| row_like | 148.166 | 156.852 | 1.059 | 1.059 | 1.000 | **slower** |
| row_like_count | 152.807 | 161.507 | 1.057 | 1.057 | 1.000 | **slower** |
| row_text | 277.411 | 277.448 | 1.000 | 1.000 | 1.000 |  |
| row_wide | 78.303 | 78.316 | 1.000 | 1.000 | 1.000 |  |
| row_window | 3183.708 | 3183.682 | 1.000 | 1.000 | 1.000 |  |

Runs: base pg-rowwise-Kc4lbC; candidate pg-rowwise-GjsWPn.

## sort

| case | base, M instructions | candidate, M instructions | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| distinct_agg | 488.863 | 489.128 | 1.001 | 1.001 | 0.983 |  |
| distinct_few | 500.699 | 500.940 | 1.000 | 1.000 | 1.000 |  |
| distinct_group | 990.835 | 993.052 | 1.002 | 1.002 | 1.000 |  |
| distinct_many | 798.682 | 798.918 | 1.000 | 1.000 | 1.000 |  |
| order_big | 1210.041 | 1210.019 | 1.000 | 1.000 | 1.001 |  |
| order_desc | 1247.377 | 1247.482 | 1.000 | 1.000 | 0.999 |  |
| order_few | 1103.329 | 1103.286 | 1.000 | 1.000 | 1.001 |  |
| order_few_text | 11120.092 | 11120.575 | 1.000 | 1.000 | 0.999 |  |
| order_filter | 384.995 | 385.728 | 1.002 | 1.002 | 1.000 |  |
| order_int | 1103.607 | 1103.602 | 1.000 | 1.000 | 1.000 |  |
| order_multi | 1427.848 | 1427.816 | 1.000 | 1.000 | 1.001 |  |
| order_numeric | 2509.875 | 2511.035 | 1.000 | 1.000 | 1.001 |  |
| order_out | 1241.737 | 1241.969 | 1.000 | 1.000 | 1.000 |  |
| order_sorted | 757.078 | 757.090 | 1.000 | 1.000 | 1.000 |  |
| order_text_c | 7657.336 | 7657.885 | 1.000 | 1.000 | 0.999 |  |
| order_text_out | 1663.440 | 1663.716 | 1.000 | 1.000 | 1.001 |  |
| order_wide | 1629.181 | 1629.135 | 1.000 | 1.000 | 1.000 |  |
| topn_few | 410.212 | 410.044 | 1.000 | 1.000 | 1.000 |  |
| topn_many | 721.756 | 721.945 | 1.000 | 1.000 | 1.000 |  |
| topn_numeric | 1629.936 | 1631.702 | 1.001 | 1.001 | 1.000 |  |
| topn_offset | 421.302 | 421.130 | 1.000 | 1.000 | 1.000 |  |
| topn_reverse | 1212.196 | 1212.036 | 1.000 | 1.000 | 1.000 |  |
| topn_text_c | 1347.051 | 1347.215 | 1.000 | 1.000 | 1.000 |  |

Runs: base pg-sort-auL6Oj; candidate pg-sort-t8V2rD.

A count does not move with the code's place: a flagged case runs more instructions. Compare the machine code of the functions it runs (tessera-bench --disasm --module nodes --function NAME); a change that trades instructions for memory is judged by time.

