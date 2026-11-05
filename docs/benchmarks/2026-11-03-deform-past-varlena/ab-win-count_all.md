# A/B: 46a8957f22dea0552ff0ccc5ae257e733c09430b against 06297a9f265280e681b09129548bf61e133cfb71

PostgreSQL 20 (a release build of master); 3 round(s) a side, base and candidate alternating; cases ^count_all$; repetitions 31; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's time over the base's; below 1 is faster. `min` compares the least time of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 2% by its minimum and by its median beyond the control.

## win

| case | base, ms | candidate, ms | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| count_all | 5.588 | 5.585 | 1.007 | 1.000 | 0.986 |  |

Runs: base pg-win-pBELT1, pg-win-RksQ2U, pg-win-ssLmNU; candidate pg-win-GdVn0I, pg-win-tbhFnr, pg-win-f6FtJI.

