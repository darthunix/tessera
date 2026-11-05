# A/B: 37cfba07aacad748e54911d86a77d7ee0bf68ff9 against 9c8b2953e2040c07e067ae6d2d247ab8852322ea

PostgreSQL 20 (/private/tmp/claude-501/-Users-darthunix-git-tessera/2dc6b822-803f-4d35-9c2a-a0ed4421e78f/scratchpad/pgmaster/bin/pg_config); 2 round(s) a side, base and candidate alternating; cases ^(pk|pk_shuffled|scattered|text_key|date_key|cross_type)$; repetitions 7; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's time over the base's; below 1 is faster. `min` compares the least time of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 2% by its minimum and by its median beyond the control.

## indexjoin

| case | base, ms | candidate, ms | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| cross_type | 1.699 | 1.592 | 1.003 | 0.937 | 0.991 |  |
| date_key | 1.619 | 1.647 | 1.009 | 1.017 | 0.991 |  |
| pk | 1.625 | 1.649 | 1.014 | 1.015 | 0.999 |  |
| pk_shuffled | 4.307 | 4.212 | 0.984 | 0.978 | 0.980 |  |
| scattered | 2.149 | 2.139 | 0.995 | 0.995 | 0.984 |  |
| text_key | 4.755 | 4.796 | 1.008 | 1.009 | 0.984 |  |

Runs: base pg-indexjoin-W47lsE, pg-indexjoin-mYPuj3; candidate pg-indexjoin-LwE8mQ, pg-indexjoin-3gvXZU.

