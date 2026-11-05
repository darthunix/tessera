# A/B: 46a8957f22dea0552ff0ccc5ae257e733c09430b against 06297a9f265280e681b09129548bf61e133cfb71

PostgreSQL 20 (a release build of master); 3 round(s) a side, base and candidate alternating; cases ^mixed_limit$; repetitions 101; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's time over the base's; below 1 is faster. `min` compares the least time of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 2% by its minimum and by its median beyond the control.

## tax

| case | base, ms | candidate, ms | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| mixed_limit | 2.868 | 2.991 | 1.027 | 1.043 | 1.054 |  |

Runs: base pg-tax-er1I7E, pg-tax-D6LsvC, pg-tax-SKXS5m; candidate pg-tax-TTbvab, pg-tax-Eg6GwT, pg-tax-GgfcRi.

