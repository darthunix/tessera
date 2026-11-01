# A/B: a09534753517dc7e526d9001b2dabb96b653089c against fd35c5af2fe585bfd1a97895af23e43ed755453c

PostgreSQL 20 (a copy of the release build of master 0497318e442); 2 round(s) a side, base and candidate alternating; cases ^(dim|dim_left|dim_anti|mixed_text|outer_text|group_fk|group_mixed)_4mb$; repetitions 11; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's time over the base's; below 1 is faster. `min` compares the least time of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 2% by its minimum and by its median beyond the control.

## spill

| case | base, ms | candidate, ms | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| dim_4mb | 28.043 | 28.678 | 1.011 | 1.023 | 1.051 |  |
| dim_anti_4mb | 25.277 | 24.061 | 1.008 | 0.952 | 1.030 |  |
| dim_left_4mb | 29.305 | 30.988 | 0.998 | 1.057 | 0.456 |  |
| group_fk_4mb | 30.551 | 32.938 | 0.993 | 1.078 | 0.946 |  |
| group_mixed_4mb | 32.227 | 28.630 | 0.935 | 0.888 | 0.739 |  |
| mixed_text_4mb | 93.659 | 89.650 | 0.991 | 0.957 | 0.900 |  |
| outer_text_4mb | 36.750 | 33.614 | 0.971 | 0.915 | 1.079 | faster |

Runs: base pg-spill-DAsI4B, pg-spill-LqAyZl; candidate pg-spill-zhIfj3, pg-spill-rzAojQ.

