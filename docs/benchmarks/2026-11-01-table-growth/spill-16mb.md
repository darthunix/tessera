# A/B: a09534753517dc7e526d9001b2dabb96b653089c against fd35c5af2fe585bfd1a97895af23e43ed755453c

PostgreSQL 20 (a copy of the release build of master 0497318e442); 2 round(s) a side, base and candidate alternating; cases ^(dim|dim_left|dim_anti|mixed_text|outer_text|group_fk|group_mixed)_16mb$; repetitions 11; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's time over the base's; below 1 is faster. `min` compares the least time of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 2% by its minimum and by its median beyond the control.

## spill

| case | base, ms | candidate, ms | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| dim_16mb | 30.401 | 27.534 | 0.981 | 0.906 | 0.907 |  |
| dim_anti_16mb | 35.700 | 22.853 | 0.998 | 0.640 | 0.862 |  |
| dim_left_16mb | 29.922 | 29.773 | 1.012 | 0.995 | 0.770 |  |
| group_fk_16mb | 30.691 | 30.423 | 0.978 | 0.991 | 0.927 |  |
| group_mixed_16mb | 16.366 | 15.146 | 0.985 | 0.925 | 0.937 |  |
| mixed_text_16mb | 63.946 | 80.796 | 1.356 | 1.263 | 0.778 | **slower** |
| outer_text_16mb | 18.569 | 18.300 | 1.006 | 0.986 | 1.063 |  |

Runs: base pg-spill-vplmFD, pg-spill-nE2Yl4; candidate pg-spill-6QyY6F, pg-spill-fZDy23.

Measure a flagged case again alone and longer (--cases, --repetitions 101); if it stays, --copt=-falign-functions=64 tells whether it is the code's place.
