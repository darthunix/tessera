# A/B: fe7b8698d3c92b20e97858236635e94b97304028 against 7978d70a873e76b7e819bd712d4e3839d4bd622c

PostgreSQL 20 (a release build of master); 2 round(s) a side, base and candidate alternating; cases ^any_(join_text|join_two|join_semi|text_few|text_col|text_agg)$; repetitions 11; workers 0; COPT none; Now drawing from 'Battery Power'.

Each ratio is the candidate's time over the base's; below 1 is faster. `min` compares the least time of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 2% by its minimum and by its median beyond the control.

## anykey

| case | base, ms | candidate, ms | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| any_join_semi | 81.465 | 70.660 | 0.993 | 0.867 | 0.953 |  |
| any_join_text | 66.541 | 64.827 | 0.985 | 0.974 | 0.952 |  |
| any_join_two | 77.019 | 75.664 | 0.985 | 0.982 | 0.986 |  |
| any_text_agg | 15.720 | 14.539 | 1.076 | 0.925 | 1.011 |  |
| any_text_col | 27.335 | 27.825 | 1.032 | 1.018 | 1.011 |  |
| any_text_few | 12.779 | 16.222 | 1.071 | 1.269 | 1.027 | **slower** |

Runs: base pg-anykey-G5TfU4, pg-anykey-RdLSQx; candidate pg-anykey-AhSajy, pg-anykey-m5OM7b.

Measure a flagged case again alone and longer (--cases, --repetitions 101); if it stays, --copt=-falign-functions=64 tells whether it is the code's place.
