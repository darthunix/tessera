# A/B: 54a613dab959736a13e59b5c960356203e4c09c1 against 094ae1185f0115a34a07544ea6c0224411949905

PostgreSQL 20 (a copy of the release build of master 0497318e442); 2 round(s) a side, base and candidate alternating; cases ^distinct_128kb$; repetitions 11; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's time over the base's; below 1 is faster. `min` compares the least time of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 2% by its minimum and by its median beyond the control.

## spill

| case | base, ms | candidate, ms | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| distinct_128kb | 43.912 | 42.812 | 0.958 | 0.975 | 0.998 | faster |

Runs: base pg-spill-cz6Rb3, pg-spill-bpCsrK; candidate pg-spill-GmZeHA, pg-spill-m6IlBo.

