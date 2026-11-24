# A/B: WORKTREE@81f23392b5a8e5139082c58cd8e44ebf96b278a3 against f8ee89ef1999c388312d20b874fb65ef76c92d03

PostgreSQL 19 (/private/tmp/claude-501/-Users-darthunix-git-tessera/b6a5d83c-0cf8-4b12-9221-e9e15b8eaa8d/scratchpad/pg19-ab/bin/pg_config); instructions counted by pg_pmu; 1 round(s) a side, base and candidate alternating; cases all; repetitions 3; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's count of instructions over the base's; below 1 is less. `min` compares the least count of instructions of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 1% by its minimum and by its median beyond the control.

`disk` compares the kB the plan's nodes report written to disk (Disk Usage, which the core's sort and hash join do not print), where either side wrote any, and is flagged past the same threshold where every count of a side wrote the same; with parallel workers it varies and is not judged. A case is counted fairly only where all its counts on both sides ran one plan shape: EXPLAIN ANALYZE counts the rows of a row-wise node one by one and of a batch node batch by batch. A case whose plans differ has no ratio and is marked `plan differs`, and one whose control's plans differ has no control.

## anykey

| case | base, M instructions | candidate, M instructions | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| any_distinct | 391.344 | 393.062 | 1.004 | 1.004 | 1.000 |  |
| any_join_numeric | 1981.239 | 1982.987 | 1.001 | 1.001 | 0.999 |  |
| any_join_semi | 1469.146 | 1469.216 | 1.000 | 1.000 | 1.000 |  |
| any_join_text | 1352.790 | 1358.806 | 1.004 | 1.004 | 1.000 |  |
| any_join_two | 1673.197 | 1679.101 | 1.004 | 1.004 | 1.000 |  |
| any_numeric | 456.181 | 456.184 | 1.001 | 1.000 | 1.000 |  |
| any_text_agg | 384.653 | 385.639 | 1.005 | 1.003 | 0.999 |  |
| any_text_col | 483.011 | 483.338 | 1.000 | 1.001 | 0.998 |  |
| any_text_few | 357.534 | 359.388 | 1.005 | 1.005 | 1.000 |  |
| any_union | 755.154 | 758.044 | 1.004 | 1.004 | 1.000 |  |

Runs: base pg-anykey-yB9o4k; candidate pg-anykey-DKxVZc.

## rowwise

| case | base, M instructions | candidate, M instructions | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| row_column | 379.680 | 379.958 | 1.001 | 1.001 | 1.000 |  |
| row_expr | 439.620 | 440.750 | 1.003 | 1.003 | 1.000 |  |
| row_first | 0.049 | 0.049 | 1.008 | 1.008 | 1.014 |  |
| row_like | 148.158 | 149.795 | 1.011 | 1.011 | 1.000 | **slower** |
| row_like_count | 152.805 | 154.411 | 1.010 | 1.011 | 1.000 | **slower** |
| row_text | 277.404 | 277.468 | 1.000 | 1.000 | 1.000 |  |
| row_wide | 78.305 | 79.603 | 1.003 | 1.017 | 1.000 |  |
| row_window | 3183.585 | 3183.624 | 1.000 | 1.000 | 1.000 |  |

Runs: base pg-rowwise-pUEjnb; candidate pg-rowwise-Y55gzr.

A count does not move with the code's place: a flagged case runs more instructions. Compare the machine code of the functions it runs (tessera-bench --disasm --module nodes --function NAME); a change that trades instructions for memory is judged by time.

