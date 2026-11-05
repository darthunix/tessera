# A/B: 46a8957f22dea0552ff0ccc5ae257e733c09430b against 06297a9f265280e681b09129548bf61e133cfb71

PostgreSQL 20 (a release build of master); 2 round(s) a side, base and candidate alternating; cases all; repetitions the family's; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's time over the base's; below 1 is faster. `min` compares the least time of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 2% by its minimum and by its median beyond the control.

## tax

| case | base, ms | candidate, ms | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| limit_1 | 0.004 | 0.005 | 1.000 | 1.125 | 1.000 |  |
| limit_1000 | 0.009 | 0.009 | 1.000 | 1.000 | 1.025 |  |
| limit_1m | 2.958 | 3.018 | 1.018 | 1.020 | 0.993 |  |
| limit_expr | 11.027 | 11.111 | 1.016 | 1.008 | 0.999 |  |
| limit_text | 2.934 | 2.954 | 1.011 | 1.007 | 1.002 |  |
| mixed_limit | 2.865 | 3.056 | 1.033 | 1.067 | 0.966 | **slower** |
| offset_1m | 2.497 | 2.555 | 1.035 | 1.023 | 1.000 | **slower** |
| sort_limit | 36.553 | 36.471 | 0.995 | 0.998 | 0.992 |  |
| wide_limit | 1.151 | 1.187 | 1.014 | 1.031 | 1.011 |  |

Runs: base pg-tax-uVSs9W, pg-tax-WslJmY; candidate pg-tax-RzlKvF, pg-tax-qiPWxk.

Measure a flagged case again alone and longer (--cases, --repetitions 101); if it stays, --copt=-falign-functions=64 tells whether it is the code's place.
