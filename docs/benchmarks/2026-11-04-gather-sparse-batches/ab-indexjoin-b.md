# A/B: 37cfba07aacad748e54911d86a77d7ee0bf68ff9 against 9c8b2953e2040c07e067ae6d2d247ab8852322ea

PostgreSQL 20 (/private/tmp/claude-501/-Users-darthunix-git-tessera/2dc6b822-803f-4d35-9c2a-a0ed4421e78f/scratchpad/pgmaster/bin/pg_config); 2 round(s) a side, base and candidate alternating; cases ^(four|left_miss|sparse|few|repeated|pk_20k|hot_key)$; repetitions 7; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's time over the base's; below 1 is faster. `min` compares the least time of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 2% by its minimum and by its median beyond the control.

## indexjoin

| case | base, ms | candidate, ms | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| few | 0.855 | 0.847 | 0.996 | 0.990 | 1.003 |  |
| four | 1.140 | 1.135 | 0.995 | 0.996 | 1.004 |  |
| hot_key | 219.671 | 219.288 | 1.004 | 0.998 | 0.996 |  |
| left_miss | 2.322 | 2.325 | 1.012 | 1.002 | 1.009 |  |
| pk_20k | 4.027 | 4.009 | 0.992 | 0.995 | 0.984 |  |
| repeated | 2.487 | 2.524 | 1.008 | 1.014 | 1.015 |  |
| sparse | 2.974 | 3.000 | 0.983 | 1.009 | 1.005 |  |

Runs: base pg-indexjoin-ouyNd6, pg-indexjoin-mHK0vC; candidate pg-indexjoin-9WDhEd, pg-indexjoin-j8NcVs.

