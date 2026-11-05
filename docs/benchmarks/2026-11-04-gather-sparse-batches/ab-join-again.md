# A/B: b04868b189bdb870f1df40b22986e6406c637e46 against 9c8b2953e2040c07e067ae6d2d247ab8852322ea

PostgreSQL 20 (/private/tmp/claude-501/-Users-darthunix-git-tessera/2dc6b822-803f-4d35-9c2a-a0ed4421e78f/scratchpad/pgmaster/bin/pg_config); 2 round(s) a side, base and candidate alternating; cases ^(dup|fk_count|fk_outer_col|int8|left_nulls)$; repetitions 51; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's time over the base's; below 1 is faster. `min` compares the least time of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 2% by its minimum and by its median beyond the control.

## join

| case | base, ms | candidate, ms | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| dup | 33.584 | 34.219 | 1.018 | 1.019 | 1.014 |  |
| fk_count | 22.193 | 22.518 | 1.031 | 1.015 | 1.014 |  |
| fk_outer_col | 24.716 | 24.851 | 1.025 | 1.005 | 1.009 |  |
| int8 | 22.587 | 22.787 | 1.028 | 1.009 | 1.008 |  |
| left_nulls | 29.011 | 29.621 | 1.040 | 1.021 | 1.007 |  |
| plan_time | 0.073 | 0.073 | 1.018 | 0.999 | 1.000 |  |

Runs: base pg-join-p1lDrd, pg-join-TBrV5I; candidate pg-join-Ptl64n, pg-join-SsDrGI.

