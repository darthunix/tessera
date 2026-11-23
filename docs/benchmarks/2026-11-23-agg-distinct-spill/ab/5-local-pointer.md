# A/B: WORKTREE@784b95a72dcffa3e42e1ee1d54757ed384d79a0a against 75fb4ba4753c8065705387c7347abf3a4df5aea6

PostgreSQL 20 (/Users/darthunix/git/postgres-review/build-release/bin/pg_config); instructions counted by pg_pmu; 1 round(s) a side, base and candidate alternating; cases ^distinct_(big|two|keys)_64mb$; repetitions 3; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's count of instructions over the base's; below 1 is less. `min` compares the least count of instructions of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 1% by its minimum and by its median beyond the control.

`disk` compares the kB the plan's nodes report written to disk (Disk Usage, which the core's sort and hash join do not print), where either side wrote any, and is flagged past the same threshold where every count of a side wrote the same; with parallel workers it varies and is not judged. A case is counted fairly only where all its counts on both sides ran one plan shape: EXPLAIN ANALYZE counts the rows of a row-wise node one by one and of a batch node batch by batch. A case whose plans differ has no ratio and is marked `plan differs`, and one whose control's plans differ has no control.

## spill

| case | base, M instructions | candidate, M instructions | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| distinct_big_64mb | 2436.522 | 2449.975 | 1.005 | 1.006 | 1.000 |  |
| distinct_keys_64mb | 2931.835 | 2788.941 | 0.951 | 0.951 | 1.000 | faster |
| distinct_two_64mb | 3341.610 | 3367.523 | 1.008 | 1.008 | 1.000 |  |

Runs: base pg-spill-oXyqC6; candidate pg-spill-iGeAlW.

