# A/B: 37cfba07aacad748e54911d86a77d7ee0bf68ff9 against 9c8b2953e2040c07e067ae6d2d247ab8852322ea

PostgreSQL 20 (/private/tmp/claude-501/-Users-darthunix-git-tessera/2dc6b822-803f-4d35-9c2a-a0ed4421e78f/scratchpad/pgmaster/bin/pg_config); 2 round(s) a side, base and candidate alternating; cases ^(two_keys|residual|rows_parent|chain|semi|anti|right_anti|left_nulls|left_dup|full_join|right_join)$; repetitions 7; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's time over the base's; below 1 is faster. `min` compares the least time of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 2% by its minimum and by its median beyond the control.

## join

| case | base, ms | candidate, ms | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| anti | 22.310 | 22.714 | 1.030 | 1.018 | 1.010 |  |
| chain | 34.260 | 32.374 | 1.009 | 0.945 | 1.013 |  |
| full_join | 37.262 | 37.709 | 1.024 | 1.012 | 1.009 |  |
| left_dup | 2.832 | 2.774 | 0.986 | 0.980 | 0.979 |  |
| left_nulls | 29.075 | 29.713 | 1.031 | 1.022 | 1.002 | **slower** |
| plan_time | 0.073 | 0.075 | 1.028 | 1.028 | 1.016 |  |
| residual | 30.273 | 31.232 | 1.031 | 1.032 | 1.014 |  |
| right_anti | 24.226 | 24.685 | 1.015 | 1.019 | 1.006 |  |
| right_join | 27.955 | 28.291 | 1.004 | 1.012 | 1.003 |  |
| rows_parent | 55.482 | 55.726 | 1.004 | 1.004 | 1.021 |  |
| semi | 22.465 | 22.692 | 1.020 | 1.010 | 0.959 |  |
| two_keys | 25.479 | 26.268 | 1.045 | 1.031 | 1.015 |  |

Runs: base pg-join-0zuoXW, pg-join-FDQGEX; candidate pg-join-AcAa7C, pg-join-7GmV09.

Measure a flagged case again alone and longer (--cases, --repetitions 101); if it stays, --copt=-falign-functions=64 tells whether it is the code's place.
