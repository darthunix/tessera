# A/B: 784b95a72dcffa3e42e1ee1d54757ed384d79a0a against 75fb4ba4753c8065705387c7347abf3a4df5aea6

PostgreSQL 20 (/Users/darthunix/git/postgres-review/build-release/bin/pg_config); instructions counted by pg_pmu; 1 round(s) a side, base and candidate alternating; cases ^(distinct_[a-z]+_64mb|distinct_thought_16mb|distinct_half_4mb)$; repetitions 3; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's count of instructions over the base's; below 1 is less. `min` compares the least count of instructions of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 1% by its minimum and by its median beyond the control.

`disk` compares the kB the plan's nodes report written to disk (Disk Usage, which the core's sort and hash join do not print), where either side wrote any, and is flagged past the same threshold where every count of a side wrote the same; with parallel workers it varies and is not judged. A case is counted fairly only where all its counts on both sides ran one plan shape: EXPLAIN ANALYZE counts the rows of a row-wise node one by one and of a batch node batch by batch. A case whose plans differ has no ratio and is marked `plan differs`, and one whose control's plans differ has no control.

## spill

| case | base, M instructions | candidate, M instructions | min | median | control | disk | |
|---|---:|---:|---:|---:|---:|---:|---|
| distinct_big_64mb | 2436.490 | 2581.179 | 1.059 | 1.059 | 1.002 | – | **slower** |
| distinct_generic_64mb | 18411.850 | 3656.345 | – | – | 0.999 | – | **plan differs** |
| distinct_half_4mb | 9946.728 | 9951.224 | 0.999 | 1.000 | 1.000 | – |  |
| distinct_half_64mb | 2102.626 | 2495.469 | 1.187 | 1.187 | 0.999 | new | **slower**, **more disk** |
| distinct_keys_64mb | 2931.775 | 2920.271 | 0.996 | 0.996 | 1.000 | – |  |
| distinct_thought_16mb | 1586.781 | 1584.893 | 0.999 | 0.999 | 1.002 | – |  |
| distinct_thought_64mb | 1586.736 | 1584.983 | 0.999 | 0.999 | 1.001 | – |  |
| distinct_two_64mb | 3341.793 | 3630.377 | 1.086 | 1.086 | 1.000 | – | **slower** |

Runs: base pg-spill-e8n1wQ; candidate pg-spill-VJceoI.

A count does not move with the code's place: a flagged case runs more instructions. Compare the machine code of the functions it runs (tessera-bench --disasm --module nodes --function NAME); a change that trades instructions for memory is judged by time.

A case flagged `more disk` spills more in the candidate: each run's timings.csv has the disk of every count, and plans.txt the Disk Usage and the spilled chunks of each node.

A case marked `plan differs` ran another plan in the candidate, or more than one, so its count compares other nodes, not the change: the column plan of each run's timings.csv has the shape of every count. Time it (the same command without --counters, --cases for the case).

