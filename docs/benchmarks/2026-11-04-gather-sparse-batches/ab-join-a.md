# A/B: 37cfba07aacad748e54911d86a77d7ee0bf68ff9 against 9c8b2953e2040c07e067ae6d2d247ab8852322ea

PostgreSQL 20 (/private/tmp/claude-501/-Users-darthunix-git-tessera/2dc6b822-803f-4d35-9c2a-a0ed4421e78f/scratchpad/pgmaster/bin/pg_config); 2 round(s) a side, base and candidate alternating; cases ^(fk_count|fk_inner_col|fk_outer_col|int8|mixed|selective|filter_first|miss|dup|dup_text)$; repetitions 7; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's time over the base's; below 1 is faster. `min` compares the least time of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 2% by its minimum and by its median beyond the control.

## join

| case | base, ms | candidate, ms | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| dup | 32.907 | 33.480 | 1.028 | 1.017 | 0.996 | **slower** |
| dup_text | 5.472 | 5.524 | 1.005 | 1.010 | 0.999 |  |
| filter_first | 9.714 | 9.825 | 1.022 | 1.011 | 1.001 |  |
| fk_count | 21.818 | 22.547 | 1.022 | 1.033 | 0.987 | **slower** |
| fk_inner_col | 26.059 | 26.233 | 1.013 | 1.007 | 1.012 |  |
| fk_outer_col | 23.981 | 24.962 | 1.033 | 1.041 | 1.005 | **slower** |
| int8 | 21.939 | 22.579 | 1.034 | 1.029 | 1.003 | **slower** |
| miss | 13.496 | 13.800 | 0.978 | 1.022 | 1.010 |  |
| mixed | 21.874 | 21.916 | 0.996 | 1.002 | 1.000 |  |
| plan_time | 0.071 | 0.071 | 1.013 | 1.002 | 0.999 |  |
| selective | 15.759 | 15.245 | 0.983 | 0.967 | 1.004 |  |

Runs: base pg-join-bOI3bE, pg-join-IPmZT0; candidate pg-join-rtX0YP, pg-join-ABQo6i.

Measure a flagged case again alone and longer (--cases, --repetitions 101); if it stays, --copt=-falign-functions=64 tells whether it is the code's place.
