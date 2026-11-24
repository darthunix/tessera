# A/B: WORKTREE@81f23392b5a8e5139082c58cd8e44ebf96b278a3 against f8ee89ef1999c388312d20b874fb65ef76c92d03

PostgreSQL 19 (/private/tmp/claude-501/-Users-darthunix-git-tessera/b6a5d83c-0cf8-4b12-9221-e9e15b8eaa8d/scratchpad/pg19-ab/bin/pg_config); instructions counted by pg_pmu; 1 round(s) a side, base and candidate alternating; cases all; repetitions 3; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's count of instructions over the base's; below 1 is less. `min` compares the least count of instructions of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 1% by its minimum and by its median beyond the control.

`disk` compares the kB the plan's nodes report written to disk (Disk Usage, which the core's sort and hash join do not print), where either side wrote any, and is flagged past the same threshold where every count of a side wrote the same; with parallel workers it varies and is not judged. A case is counted fairly only where all its counts on both sides ran one plan shape: EXPLAIN ANALYZE counts the rows of a row-wise node one by one and of a batch node batch by batch. A case whose plans differ has no ratio and is marked `plan differs`, and one whose control's plans differ has no control.

## anykey

| case | base, M instructions | candidate, M instructions | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| any_distinct | 391.327 | 393.110 | 1.005 | 1.005 | 1.000 |  |
| any_join_numeric | 1983.457 | 1982.238 | 1.001 | 0.999 | 1.000 |  |
| any_join_semi | 1470.030 | 1469.186 | 1.000 | 0.999 | 1.000 |  |
| any_join_text | 1353.294 | 1359.251 | 1.005 | 1.004 | 1.000 |  |
| any_join_two | 1673.118 | 1679.633 | 1.004 | 1.004 | 1.000 |  |
| any_numeric | 455.750 | 456.171 | 1.001 | 1.001 | 1.000 |  |
| any_text_agg | 383.531 | 385.647 | 1.006 | 1.006 | 1.000 |  |
| any_text_col | 482.966 | 483.113 | 1.000 | 1.000 | 1.000 |  |
| any_text_few | 358.335 | 360.346 | 1.005 | 1.006 | 1.000 |  |
| any_union | 754.686 | 758.350 | 1.005 | 1.005 | 1.000 |  |

Runs: base pg-anykey-KFu0p9; candidate pg-anykey-i7O2qP.

## rowwise

| case | base, M instructions | candidate, M instructions | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| row_column | 379.951 | 379.946 | 1.001 | 1.000 | 1.000 |  |
| row_expr | 439.606 | 440.621 | 1.002 | 1.002 | 1.000 |  |
| row_first | 0.049 | 0.049 | 1.001 | 1.001 | 1.000 |  |
| row_like | 148.162 | 149.798 | 1.011 | 1.011 | 1.000 | **slower** |
| row_like_count | 152.807 | 154.451 | 1.011 | 1.011 | 1.000 | **slower** |
| row_text | 277.393 | 277.465 | 1.000 | 1.000 | 1.000 |  |
| row_wide | 78.321 | 78.370 | 1.000 | 1.001 | 1.000 |  |
| row_window | 3183.585 | 3183.748 | 1.000 | 1.000 | 1.000 |  |

Runs: base pg-rowwise-vr4XJV; candidate pg-rowwise-F04Wcp.

A count does not move with the code's place: a flagged case runs more instructions. Compare the machine code of the functions it runs (tessera-bench --disasm --module nodes --function NAME); a change that trades instructions for memory is judged by time.

