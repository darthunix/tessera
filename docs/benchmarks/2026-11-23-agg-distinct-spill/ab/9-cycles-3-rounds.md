# A/B: 8e126650f0cb7f551acc8687070d68cbdd9e3918 against 75fb4ba4753c8065705387c7347abf3a4df5aea6

PostgreSQL 20 (/Users/darthunix/git/postgres-review/build-release/bin/pg_config); cycles counted by pg_pmu; 3 round(s) a side, base and candidate alternating; cases ^(distinct_(big|two|keys)_64mb|distinct_half_4mb)$; repetitions 3; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's count of cycles over the base's; below 1 is less. `min` compares the least count of cycles of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 1% by its minimum and by its median beyond the control.

`disk` compares the kB the plan's nodes report written to disk (Disk Usage, which the core's sort and hash join do not print), where either side wrote any, and is flagged past the same threshold where every count of a side wrote the same; with parallel workers it varies and is not judged. A case is counted fairly only where all its counts on both sides ran one plan shape: EXPLAIN ANALYZE counts the rows of a row-wise node one by one and of a batch node batch by batch. A case whose plans differ has no ratio and is marked `plan differs`, and one whose control's plans differ has no control.

## spill

| case | base, M cycles | candidate, M cycles | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| distinct_big_64mb | 686.198 | 687.617 | 1.002 | 1.002 | 0.961 |  |
| distinct_half_4mb | 1595.194 | 1562.950 | 1.000 | 0.980 | 0.964 |  |
| distinct_keys_64mb | 765.508 | 749.523 | 0.981 | 0.979 | 0.984 |  |
| distinct_two_64mb | 959.588 | 960.173 | 1.000 | 1.001 | 0.979 |  |

Runs: base pg-spill-PzuQQ5, pg-spill-nC1Rc6, pg-spill-qDSmo6; candidate pg-spill-uKbCU7, pg-spill-d8gJDv, pg-spill-5H2rV2.

