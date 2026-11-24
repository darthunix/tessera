# A/B: WORKTREE@cb7c230a6325128b52d7da4acd45948eb24c3ade against 624937e9bffa3309c2a8968006cdf5fce2382e73

PostgreSQL 20 (/Users/darthunix/git/postgres-review/build-release/bin/pg_config); times; 1 round(s) a side, base and candidate alternating; cases digits; repetitions 11; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's time over the base's; below 1 is less. `min` compares the least time of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 2% by its minimum and by its median beyond the control.

## anyagg

| case | base, ms | candidate, ms | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| g_avg_digits18 | 4.787 | 4.383 | 0.907 | 0.916 | 1.003 | faster |
| g_avg_digits19 | 35.617 | 4.398 | 0.120 | 0.123 | 1.007 | faster |
| g_runs_digits18 | 14.764 | 14.668 | 0.996 | 0.993 | 0.985 |  |
| g_runs_digits19 | 47.753 | 14.646 | 0.304 | 0.307 | 0.993 | faster |
| g_spread_digits18 | 14.516 | 14.999 | 1.033 | 1.033 | 0.999 | **slower** |
| g_spread_digits19 | 46.630 | 14.509 | 0.309 | 0.311 | 0.994 | faster |
| g_sum_digits18 | 4.757 | 4.410 | 0.905 | 0.927 | 0.997 | faster |
| g_sum_digits19 | 35.172 | 4.421 | 0.124 | 0.126 | 1.011 | faster |

Runs: base pg-anyagg-Q5DzEN; candidate pg-anyagg-PvQHiy.

Measure a flagged case again alone and longer (--cases, --repetitions 101); if it stays, --copt=-falign-functions=64 tells whether it is the code's place.

