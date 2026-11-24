# A/B: WORKTREE@493ad71556822fcc61caa998a627c845fb5600e1 against 624937e9bffa3309c2a8968006cdf5fce2382e73

PostgreSQL 20 (/Users/darthunix/git/postgres-review/build-release/bin/pg_config); times; 3 round(s) a side, base and candidate alternating; cases digits; repetitions 11; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's time over the base's; below 1 is less. `min` compares the least time of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 2% by its minimum and by its median beyond the control.

## anyagg

| case | base, ms | candidate, ms | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| g_avg_digits18 | 4.702 | 4.353 | 0.901 | 0.926 | 1.009 | faster |
| g_avg_digits19 | 35.238 | 4.359 | 0.122 | 0.124 | 0.992 | faster |
| g_runs_digits18 | 14.740 | 14.367 | 0.992 | 0.975 | 1.003 |  |
| g_runs_digits19 | 46.880 | 14.435 | 0.306 | 0.308 | 0.990 | faster |
| g_spread_digits18 | 14.327 | 14.291 | 1.001 | 0.997 | 1.006 |  |
| g_spread_digits19 | 46.349 | 14.254 | 0.307 | 0.308 | 1.004 | faster |
| g_sum_digits18 | 4.679 | 4.462 | 0.920 | 0.954 | 1.001 | faster |
| g_sum_digits19 | 34.872 | 4.296 | 0.121 | 0.123 | 1.011 | faster |

Runs: base pg-anyagg-zZFDHM, pg-anyagg-FbRsRW, pg-anyagg-aPCAGH; candidate pg-anyagg-gjaz3z, pg-anyagg-4a55Tl, pg-anyagg-3k5LEw.

