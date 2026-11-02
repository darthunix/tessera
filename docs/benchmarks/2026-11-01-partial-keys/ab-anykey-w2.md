# A/B: WORKTREE@27effe4683d652a1658bbbff3b8c093d58fdef7a against ee33f2e4b429b5c08207c482cb2c936c77406d96

PostgreSQL 20 (a release build of master); 2 round(s) a side, base and candidate alternating; cases all; repetitions the family's; workers 2; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's time over the base's; below 1 is faster. `min` compares the least time of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 2% by its minimum and by its median beyond the control.

## anykey

| case | base, ms | candidate, ms | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| any_distinct | 14.131 | 14.831 | 1.049 | 1.049 | 1.014 | **slower** |
| any_join_numeric | 38.933 | 39.323 | 1.006 | 1.010 | 0.885 |  |
| any_join_semi | 29.419 | 28.495 | 0.989 | 0.969 | 1.012 |  |
| any_join_text | 33.276 | 34.060 | 1.028 | 1.024 | 0.994 | **slower** |
| any_join_two | 37.001 | 36.979 | 0.996 | 0.999 | 0.994 |  |
| any_numeric | 15.652 | 16.063 | 1.000 | 1.026 | 1.032 |  |
| any_text_agg | 13.660 | 13.869 | 1.030 | 1.015 | 0.988 | **slower** |
| any_text_col | 25.379 | 27.881 | 0.997 | 1.099 | 0.967 |  |
| any_text_few | 12.593 | 13.181 | 1.008 | 1.047 | 1.007 |  |
| any_union | 26.493 | 27.257 | 1.006 | 1.029 | 0.996 |  |

Runs: base pg-anykey-w2-cUnO3L, pg-anykey-w2-FqsJKV; candidate pg-anykey-w2-DtK7Is, pg-anykey-w2-3qPCHK.

Measure a flagged case again alone and longer (--cases, --repetitions 101); if it stays, --copt=-falign-functions=64 tells whether it is the code's place.
