# A/B: 46a8957f22dea0552ff0ccc5ae257e733c09430b against 06297a9f265280e681b09129548bf61e133cfb71

PostgreSQL 20 (/private/tmp/claude-501/-Users-darthunix-git-tessera/2dc6b822-803f-4d35-9c2a-a0ed4421e78f/scratchpad/pgmaster/bin/pg_config); 2 round(s) a side, base and candidate alternating; cases all; repetitions the family's; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's time over the base's; below 1 is faster. `min` compares the least time of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 2% by its minimum and by its median beyond the control.

## tax

| case | base, ms | candidate, ms | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| limit_1 | 0.004 | 0.005 | 1.333 | 1.125 | 1.000 | **slower** |
| limit_1000 | 0.009 | 0.009 | 1.125 | 1.000 | 1.024 |  |
| limit_1m | 4.206 | 3.647 | 1.002 | 0.867 | 0.997 |  |
| limit_expr | 11.970 | 11.584 | 1.002 | 0.968 | 0.975 |  |
| limit_text | 3.053 | 2.963 | 0.990 | 0.971 | 0.930 |  |
| mixed_limit | 2.903 | 2.881 | 0.998 | 0.992 | 0.978 |  |
| offset_1m | 3.175 | 3.113 | 1.002 | 0.981 | 0.981 |  |
| sort_limit | 232.540 | 209.398 | 0.871 | 0.900 | 0.880 |  |
| wide_limit | 1.185 | 1.180 | 1.000 | 0.996 | 0.961 |  |

Runs: base pg-tax-LJ3EXU, pg-tax-5OUapI; candidate pg-tax-FP6btJ, pg-tax-u1iwbG.

Measure a flagged case again alone and longer (--cases, --repetitions 101); if it stays, --copt=-falign-functions=64 tells whether it is the code's place.

# A/B: 46a8957f22dea0552ff0ccc5ae257e733c09430b against 06297a9f265280e681b09129548bf61e133cfb71

PostgreSQL 20 (/private/tmp/claude-501/-Users-darthunix-git-tessera/2dc6b822-803f-4d35-9c2a-a0ed4421e78f/scratchpad/pgmaster/bin/pg_config); 3 round(s) a side, base and candidate alternating; cases ^limit_1$; repetitions 101; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's time over the base's; below 1 is faster. `min` compares the least time of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 2% by its minimum and by its median beyond the control.

## tax

| case | base, ms | candidate, ms | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| limit_1 | 0.004 | 0.004 | 1.000 | 1.000 | 1.000 |  |

Runs: base pg-tax-hb1NFc, pg-tax-ag1yFM, pg-tax-nsMUqi; candidate pg-tax-OCdPSp, pg-tax-JxJytV, pg-tax-BCqOXh.

