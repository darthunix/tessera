# A/B: 37cfba07aacad748e54911d86a77d7ee0bf68ff9 against 9c8b2953e2040c07e067ae6d2d247ab8852322ea

PostgreSQL 20 (/private/tmp/claude-501/-Users-darthunix-git-tessera/2dc6b822-803f-4d35-9c2a-a0ed4421e78f/scratchpad/pgmaster/bin/pg_config); 2 round(s) a side, base and candidate alternating; cases ^(merge_forced|merge_sorted|part_prune|part_prune_few|plan_time)$; repetitions 7; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's time over the base's; below 1 is faster. `min` compares the least time of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 2% by its minimum and by its median beyond the control.

## join

| case | base, ms | candidate, ms | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| merge_forced | 227.299 | 207.678 | 0.992 | 0.914 | 0.935 |  |
| merge_sorted | 41.094 | 39.878 | 1.006 | 0.970 | 0.914 |  |
| part_prune | 5.724 | 5.460 | 0.982 | 0.954 | 0.977 |  |
| part_prune_few | 3.894 | 3.931 | 0.997 | 1.009 | 0.979 |  |
| plan_time | 0.076 | 0.074 | 1.008 | 0.974 | 0.952 |  |

Runs: base pg-join-wlgDlP, pg-join-v1Y7wA; candidate pg-join-Repi62, pg-join-G1nRyl.

