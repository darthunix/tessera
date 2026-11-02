# A/B: d1250e5592e4c38ca23f79a83e2e8108c061d2d2 against 4b905b622e615cca18991c2d651effdc74cdf84a

PostgreSQL 20 (a release build of master); 2 round(s) a side, base and candidate alternating; cases ^(right_anti|anti|semi|left_nulls|right_join|full_join)$; repetitions the family's; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's time over the base's; below 1 is faster. `min` compares the least time of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 2% by its minimum and by its median beyond the control.

## join

| case | base, ms | candidate, ms | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| anti | 22.834 | 22.871 | 1.008 | 1.002 | 1.004 |  |
| full_join | 38.627 | 37.706 | 0.992 | 0.976 | 0.995 |  |
| left_nulls | 29.852 | 29.913 | 1.015 | 1.002 | 1.011 |  |
| plan_time | 0.071 | 0.070 | 0.993 | 0.997 | 0.984 |  |
| right_anti | 93.772 | 25.597 | 0.266 | 0.273 | 1.002 | faster |
| right_join | 28.425 | 28.992 | 1.015 | 1.020 | 1.023 |  |
| semi | 22.611 | 22.821 | 1.014 | 1.009 | 1.001 |  |

Runs: base pg-join-XPxHPy, pg-join-XGIQmC; candidate pg-join-9TDDoK, pg-join-pWfiwY.

