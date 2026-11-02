# A/B: WORKTREE@27effe4683d652a1658bbbff3b8c093d58fdef7a against ee33f2e4b429b5c08207c482cb2c936c77406d96

PostgreSQL 20 (a release build of master); 3 round(s) a side, base and candidate alternating; cases ^any_(distinct|text_agg|join_text)$; repetitions 101; workers 2; COPT none; Now drawing from 'Battery Power'.

Each ratio is the candidate's time over the base's; below 1 is faster. `min` compares the least time of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 2% by its minimum and by its median beyond the control.

## anykey

| case | base, ms | candidate, ms | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| any_distinct | 14.119 | 14.977 | 0.997 | 1.061 | 0.993 |  |
| any_join_text | 33.164 | 35.560 | 0.996 | 1.072 | 0.988 |  |
| any_text_agg | 13.344 | 13.503 | 0.998 | 1.012 | 1.011 |  |

Runs: base pg-anykey-w2-6SY2Lu, pg-anykey-w2-2ZBS8q, pg-anykey-w2-Z3mFSP; candidate pg-anykey-w2-JpGq1F, pg-anykey-w2-cmk9rs, pg-anykey-w2-Ey2Kxn.

