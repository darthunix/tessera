# A/B: WORKTREE@016d310806f173306fceb251cb6a0a120dacabd9 against 081173b18f0bb835744ee2ebe4dbe337c1acb4d0

PostgreSQL 20 (/Users/darthunix/git/postgres-review/build-release/bin/pg_config); 2 round(s) a side, base and candidate alternating; cases ^(runs_text|rows_text|rows_generic)$; repetitions 51; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's time over the base's; below 1 is faster. `min` compares the least time of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 2% by its minimum and by its median beyond the control.

## exec

| case | base, ms | candidate, ms | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| rows_generic | 59.075 | 54.968 | 0.913 | 0.930 | 1.069 | faster |
| rows_text | 45.269 | 44.514 | 1.005 | 0.983 | 0.903 |  |
| runs_text | 125.587 | 126.834 | 1.014 | 1.010 | 1.095 |  |

Runs: base pg-exec-VH6utr, pg-exec-f5REUO; candidate pg-exec-kYzsJI, pg-exec-p999xF.

