# A/B: 8e126650f0cb7f551acc8687070d68cbdd9e3918 against 75fb4ba4753c8065705387c7347abf3a4df5aea6

PostgreSQL 20 (/Users/darthunix/git/postgres-review/build-release/bin/pg_config); cycles counted by pg_pmu; 1 round(s) a side, base and candidate alternating; cases ^(distinct_[a-z]+_64mb|distinct_thought_16mb|distinct_half_4mb)$; repetitions 3; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's count of cycles over the base's; below 1 is less. `min` compares the least count of cycles of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 1% by its minimum and by its median beyond the control.

`disk` compares the kB the plan's nodes report written to disk (Disk Usage, which the core's sort and hash join do not print), where either side wrote any, and is flagged past the same threshold where every count of a side wrote the same; with parallel workers it varies and is not judged. A case is counted fairly only where all its counts on both sides ran one plan shape: EXPLAIN ANALYZE counts the rows of a row-wise node one by one and of a batch node batch by batch. A case whose plans differ has no ratio and is marked `plan differs`, and one whose control's plans differ has no control.

## spill

| case | base, M cycles | candidate, M cycles | min | median | control | disk | |
|---|---:|---:|---:|---:|---:|---:|---|
| distinct_big_64mb | 703.545 | 695.436 | 0.988 | 0.988 | 0.949 | – |  |
| distinct_generic_64mb | 3325.234 | 848.525 | – | – | 0.955 | – | **plan differs** |
| distinct_half_4mb | 1716.248 | 1562.735 | 0.939 | 0.911 | 0.902 | – |  |
| distinct_half_64mb | 750.137 | 831.207 | 1.114 | 1.108 | 0.986 | new | **slower**, **more disk** |
| distinct_keys_64mb | 787.506 | 762.387 | 0.964 | 0.968 | 0.976 | – |  |
| distinct_thought_16mb | 350.891 | 326.951 | 0.937 | 0.932 | 0.971 | – | faster |
| distinct_thought_64mb | 349.138 | 332.702 | 0.958 | 0.953 | 0.990 | – | faster |
| distinct_two_64mb | 986.255 | 976.003 | 0.984 | 0.990 | 0.966 | – |  |

Runs: base pg-spill-ZVSZan; candidate pg-spill-WdLnbF.

Cycles move with memory, the processor's state and the code's place, as a time does: count the instructions of the case too (--counters without --cycles), and where they did not move, compare the machine code's places (tessera-bench --disasm --module nodes --function NAME).

A case flagged `more disk` spills more in the candidate: each run's timings.csv has the disk of every count, and plans.txt the Disk Usage and the spilled chunks of each node.

A case marked `plan differs` ran another plan in the candidate, or more than one, so its count compares other nodes, not the change: the column plan of each run's timings.csv has the shape of every count. Time it (the same command without --counters, --cases for the case).

