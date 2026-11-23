# A/B: 8e126650f0cb7f551acc8687070d68cbdd9e3918 against 75fb4ba4753c8065705387c7347abf3a4df5aea6

PostgreSQL 20 (/Users/darthunix/git/postgres-review/build-release/bin/pg_config); instructions counted by pg_pmu; 1 round(s) a side, base and candidate alternating; cases ^(distinct_[a-z]+_64mb|distinct_thought_16mb|distinct_half_4mb)$; repetitions 3; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's count of instructions over the base's; below 1 is less. `min` compares the least count of instructions of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 1% by its minimum and by its median beyond the control.

`disk` compares the kB the plan's nodes report written to disk (Disk Usage, which the core's sort and hash join do not print), where either side wrote any, and is flagged past the same threshold where every count of a side wrote the same; with parallel workers it varies and is not judged. A case is counted fairly only where all its counts on both sides ran one plan shape: EXPLAIN ANALYZE counts the rows of a row-wise node one by one and of a batch node batch by batch. A case whose plans differ has no ratio and is marked `plan differs`, and one whose control's plans differ has no control.

## spill

| case | base, M instructions | candidate, M instructions | min | median | control | disk | |
|---|---:|---:|---:|---:|---:|---:|---|
| distinct_big_64mb | 2436.451 | 2449.856 | 1.005 | 1.006 | 0.999 | – |  |
| distinct_generic_64mb | 18458.089 | 3525.025 | – | – | 1.000 | – | **plan differs** |
| distinct_half_4mb | 9950.503 | 9949.090 | 1.000 | 1.000 | 0.999 | – |  |
| distinct_half_64mb | 2102.375 | 2398.030 | 1.141 | 1.141 | 0.998 | new | **slower**, **more disk** |
| distinct_keys_64mb | 2931.715 | 2788.794 | 0.951 | 0.951 | 1.001 | – | faster |
| distinct_thought_16mb | 1586.733 | 1503.312 | 0.947 | 0.947 | 1.000 | – | faster |
| distinct_thought_64mb | 1586.704 | 1503.413 | 0.947 | 0.948 | 1.000 | – | faster |
| distinct_two_64mb | 3341.796 | 3367.709 | 1.008 | 1.008 | 1.001 | – |  |

Runs: base pg-spill-SEiTy3; candidate pg-spill-pBXqP3.

A count does not move with the code's place: a flagged case runs more instructions. Compare the machine code of the functions it runs (tessera-bench --disasm --module nodes --function NAME); a change that trades instructions for memory is judged by time.

A case flagged `more disk` spills more in the candidate: each run's timings.csv has the disk of every count, and plans.txt the Disk Usage and the spilled chunks of each node.

A case marked `plan differs` ran another plan in the candidate, or more than one, so its count compares other nodes, not the change: the column plan of each run's timings.csv has the shape of every count. Time it (the same command without --counters, --cases for the case).

