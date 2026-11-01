# A/B: 155fc260a0795745ff5358d8ab184ef8f1a6923f against 510029a06eb952c0f85a97f68ce8ac19dbfaf754

PostgreSQL 20 (a copy of the release build of master 0497318e442); 2 round(s) a side, base and candidate alternating; cases ^group_fk_1536kb$; repetitions 11; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's time over the base's; below 1 is faster. `min` compares the least time of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 2% by its minimum and by its median beyond the control.

## spill

| case | base, ms | candidate, ms | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| group_fk_1536kb | 85.565 | 85.352 | 1.001 | 0.998 | 1.011 |  |

Runs: base pg-spill-s4IXkQ, pg-spill-F4kthA; candidate pg-spill-MwndrT, pg-spill-Qo96vR.

