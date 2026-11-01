# A/B: a09534753517dc7e526d9001b2dabb96b653089c against fd35c5af2fe585bfd1a97895af23e43ed755453c

PostgreSQL 20 (a copy of the release build of master 0497318e442); 2 round(s) a side, base and candidate alternating; cases ^(dim|dim_left|dim_anti|mixed_text|outer_text|group_fk|group_mixed)_64mb$; repetitions 11; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's time over the base's; below 1 is faster. `min` compares the least time of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 2% by its minimum and by its median beyond the control.

## spill

| case | base, ms | candidate, ms | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| dim_64mb | 28.129 | 29.108 | 1.007 | 1.035 | 1.006 |  |
| dim_anti_64mb | 22.521 | 22.970 | 1.027 | 1.020 | 1.013 |  |
| dim_left_64mb | 29.569 | 30.448 | 0.995 | 1.030 | 1.028 |  |
| group_fk_64mb | 29.886 | 30.129 | 0.985 | 1.008 | 1.046 |  |
| group_mixed_64mb | 14.831 | 17.044 | 1.076 | 1.149 | 1.313 |  |
| mixed_text_64mb | 58.605 | 59.120 | 0.975 | 1.009 | 1.095 | faster |
| outer_text_64mb | 18.633 | 21.258 | 0.987 | 1.141 | 1.083 |  |

Runs: base pg-spill-tncjwt, pg-spill-mFBQHh; candidate pg-spill-4keYcx, pg-spill-kNQpxC.

