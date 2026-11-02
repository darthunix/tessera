# A/B: 44ba88868d4b399338b1c81ff6279cf97dc74463 against 7978d70a873e76b7e819bd712d4e3839d4bd622c

PostgreSQL 20 (a release build of master); 3 round(s) a side, base and candidate alternating; cases ^any_(text_few|text_col|text_agg|join_text|join_semi)$; repetitions 51; workers 0; COPT none; Now drawing from 'Battery Power'.

Each ratio is the candidate's time over the base's; below 1 is faster. `min` compares the least time of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 2% by its minimum and by its median beyond the control.

## anykey

| case | base, ms | candidate, ms | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| any_join_semi | 71.759 | 73.309 | 1.024 | 1.022 | 1.000 | **slower** |
| any_join_text | 67.571 | 66.521 | 0.959 | 0.984 | 1.020 | faster |
| any_text_agg | 13.586 | 14.904 | 1.031 | 1.097 | 1.011 | **slower** |
| any_text_col | 27.393 | 27.004 | 0.990 | 0.986 | 0.938 |  |
| any_text_few | 12.928 | 13.129 | 1.025 | 1.016 | 0.994 | **slower** |

Runs: base pg-anykey-EKB6fT, pg-anykey-aiHjZh, pg-anykey-Rv10Lz; candidate pg-anykey-FyN1ZD, pg-anykey-1WeieL, pg-anykey-a6H1rp.

Measure a flagged case again alone and longer (--cases, --repetitions 101); if it stays, --copt=-falign-functions=64 tells whether it is the code's place.
