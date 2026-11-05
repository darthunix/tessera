# A/B: 46a8957f22dea0552ff0ccc5ae257e733c09430b against 06297a9f265280e681b09129548bf61e133cfb71

PostgreSQL 20 (a release build of master); 3 round(s) a side, base and candidate alternating; cases ^offset_1m$; repetitions 101; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's time over the base's; below 1 is faster. `min` compares the least time of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 2% by its minimum and by its median beyond the control.

## tax

| case | base, ms | candidate, ms | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| offset_1m | 2.630 | 2.583 | 1.008 | 0.982 | 0.992 |  |

Runs: base pg-tax-oG5NFF, pg-tax-uAltCb, pg-tax-NJ68DD; candidate pg-tax-ZlL0Yp, pg-tax-ADzcQg, pg-tax-IPZMG6.

