# A/B: 5220f5090729f1429e9f5dfdcd15d1be70e1a218 against 481911ee83200af65e350ae05c36e9290ce00262

PostgreSQL 20 (a release build of master); 3 round(s) a side, base and candidate alternating; cases ^semi_pk$; repetitions 31; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's time over the base's; below 1 is faster. `min` compares the least time of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 2% by its minimum and by its median beyond the control.

## indexjoin

| case | base, ms | candidate, ms | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| semi_pk | 1.525 | 1.583 | 0.994 | 1.038 | 1.000 |  |

Runs: base pg-indexjoin-AbOHjJ, pg-indexjoin-vfvwOO, pg-indexjoin-9JJJeC; candidate pg-indexjoin-IS9Zwp, pg-indexjoin-kblU9b, pg-indexjoin-uBswrM.

