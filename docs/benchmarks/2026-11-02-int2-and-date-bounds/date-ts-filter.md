# A/B: 9356c3fb80a5f3246b769dc20e6589a77472e7e0 against c14180a55349d8f424f503dd74766c276b9b01be

PostgreSQL 20 (a copy of the release build of master 0497318e442); 2 round(s) a side, base and candidate alternating; cases ^date_ts_filter$; repetitions 101; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's time over the base's; below 1 is faster. `min` compares the least time of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 2% by its minimum and by its median beyond the control.

## win

| case | base, ms | candidate, ms | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| date_ts_filter | 7.943 | 8.160 | 1.012 | 1.027 | 1.008 |  |

Runs: base pg-win-knAbxF, pg-win-IveY27; candidate pg-win-arDoWU, pg-win-Yw25II.

