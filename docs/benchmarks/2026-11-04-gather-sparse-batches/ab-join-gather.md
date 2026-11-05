# A/B: b04868b189bdb870f1df40b22986e6406c637e46 against 9c8b2953e2040c07e067ae6d2d247ab8852322ea

PostgreSQL 20 (/private/tmp/claude-501/-Users-darthunix-git-tessera/2dc6b822-803f-4d35-9c2a-a0ed4421e78f/scratchpad/pgmaster/bin/pg_config); 2 round(s) a side, base and candidate alternating; cases ^gather_; repetitions 7; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's time over the base's; below 1 is faster. `min` compares the least time of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 2% by its minimum and by its median beyond the control.

## join

| case | base, ms | candidate, ms | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| gather_computed | 13.646 | 13.713 | 1.008 | 1.005 | 1.002 |  |
| gather_numeric | 18.374 | 17.791 | 0.951 | 0.968 | 0.999 | faster |
| gather_sparse | 17.163 | 14.662 | 0.861 | 0.854 | 1.002 | faster |
| gather_wide_few | 3.743 | 3.689 | 0.983 | 0.986 | 0.994 |  |
| plan_time | 0.073 | 0.074 | 1.005 | 1.015 | 1.003 |  |

Runs: base pg-join-6ZFARb, pg-join-4JZoOC; candidate pg-join-6Uw09v, pg-join-kNbSmd.

