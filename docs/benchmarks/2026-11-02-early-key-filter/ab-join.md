# A/B: b93dce4c2429c259dd07cac94706d98c775de240 against 16128ec266960a4964e2d7f1d950715fcc881839

PostgreSQL 20 (a release build of master); 2 round(s) a side, base and candidate alternating; cases ^(selective|filter_first|miss|semi|anti|fk_count|right_join)$; repetitions the family's; workers 0; COPT none; Now drawing from 'Battery Power'.

Each ratio is the candidate's time over the base's; below 1 is faster. `min` compares the least time of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 2% by its minimum and by its median beyond the control.

## join

| case | base, ms | candidate, ms | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| anti | 22.352 | 22.487 | 1.007 | 1.006 | 1.006 |  |
| filter_first | 10.038 | 9.875 | 0.992 | 0.984 | 1.010 |  |
| fk_count | 22.523 | 22.436 | 1.005 | 0.996 | 1.009 |  |
| miss | 14.965 | 14.189 | 0.949 | 0.948 | 0.923 |  |
| plan_time | 0.075 | 0.071 | 1.008 | 0.937 | 1.002 |  |
| right_join | 27.857 | 27.969 | 1.008 | 1.004 | 1.001 |  |
| selective | 15.337 | 15.322 | 0.994 | 0.999 | 1.000 |  |
| semi | 23.489 | 22.658 | 0.992 | 0.965 | 1.019 |  |

Runs: base pg-join-oLSog3, pg-join-asAdOS; candidate pg-join-YvhDvP, pg-join-sZt2wC.

