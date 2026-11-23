# A/B: 8e126650f0cb7f551acc8687070d68cbdd9e3918 against 75fb4ba4753c8065705387c7347abf3a4df5aea6

PostgreSQL 20 (/Users/darthunix/git/postgres-review/build-release/bin/pg_config); cycles counted by pg_pmu; 1 round(s) a side, base and candidate alternating; cases ^(distinct_[a-z]+_64mb|distinct_thought_16mb|distinct_half_4mb)$; repetitions 3; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's count of cycles over the base's; below 1 is less. `min` compares the least count of cycles of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 1% by its minimum and by its median beyond the control.

`disk` compares the kB the plan's nodes report written to disk (Disk Usage, which the core's sort and hash join do not print), where either side wrote any, and is flagged past the same threshold where every count of a side wrote the same; with parallel workers it varies and is not judged. A case is counted fairly only where all its counts on both sides ran one plan shape: EXPLAIN ANALYZE counts the rows of a row-wise node one by one and of a batch node batch by batch. A case whose plans differ has no ratio and is marked `plan differs`, and one whose control's plans differ has no control.

## spill

| case | base, M cycles | candidate, M cycles | min | median | control | disk | |
|---|---:|---:|---:|---:|---:|---:|---|
| distinct_big_64mb | 693.453 | 690.671 | 0.995 | 0.996 | 0.993 | – |  |
| distinct_generic_64mb | 3257.955 | 839.220 | – | – | 0.944 | – | **plan differs** |
| distinct_half_4mb | 1732.140 | 1589.527 | 0.925 | 0.918 | 0.923 | – |  |
| distinct_half_64mb | 751.266 | 827.331 | 1.095 | 1.101 | 0.950 | new | **slower**, **more disk** |
| distinct_keys_64mb | 814.850 | 750.275 | 0.924 | 0.921 | 1.062 | – | faster |
| distinct_thought_16mb | 344.057 | 320.959 | 0.942 | 0.933 | 1.085 | – | faster |
| distinct_thought_64mb | 343.934 | 327.926 | 0.945 | 0.953 | 1.057 | – | faster |
| distinct_two_64mb | 972.642 | 960.187 | 0.989 | 0.987 | 1.001 | – | faster |

Runs: base pg-spill-TlFbMP; candidate pg-spill-HDiRs7.

Cycles move with memory, the processor's state and the code's place, as a time does: count the instructions of the case too (--counters without --cycles), and where they did not move, compare the machine code's places (tessera-bench --disasm --module nodes --function NAME).

A case flagged `more disk` spills more in the candidate: each run's timings.csv has the disk of every count, and plans.txt the Disk Usage and the spilled chunks of each node.

A case marked `plan differs` ran another plan in the candidate, or more than one, so its count compares other nodes, not the change: the column plan of each run's timings.csv has the shape of every count. Time it (the same command without --counters, --cases for the case).

