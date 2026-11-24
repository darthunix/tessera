# A/B: WORKTREE@cb7c230a6325128b52d7da4acd45948eb24c3ade against 624937e9bffa3309c2a8968006cdf5fce2382e73

PostgreSQL 20 (/Users/darthunix/git/postgres-review/build-release/bin/pg_config); times; 3 round(s) a side, base and candidate alternating; cases ^g_(spread|runs)_digits18$; repetitions 21; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's time over the base's; below 1 is less. `min` compares the least time of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 2% by its minimum and by its median beyond the control.

## anyagg

| case | base, ms | candidate, ms | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| g_runs_digits18 | 14.833 | 14.629 | 0.994 | 0.986 | 1.003 |  |
| g_spread_digits18 | 14.602 | 14.711 | 1.008 | 1.007 | 1.005 |  |

Runs: base pg-anyagg-ZAcAW9, pg-anyagg-OoPLzz, pg-anyagg-kZYZGP; candidate pg-anyagg-RgHMqG, pg-anyagg-j07LIK, pg-anyagg-OF5L2b.

