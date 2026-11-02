# A/B: WORKTREE@a168afe93f6828e469639909ed0d53b64b1820f7 against c14180a55349d8f424f503dd74766c276b9b01be

PostgreSQL 20 (a copy of the release build of master 0497318e442); 2 round(s) a side, base and candidate alternating; cases ^tstz; repetitions 21; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's time over the base's; below 1 is faster. `min` compares the least time of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 2% by its minimum and by its median beyond the control.

## win

| case | base, ms | candidate, ms | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| tstz_cast | 81.768 | 135.639 | 1.650 | 1.659 | 1.007 | **slower** |
| tstz_spans | 101.885 | 241.591 | 2.360 | 2.371 | 1.017 | **slower** |
| tstz_trunc | 73.879 | 139.745 | 1.873 | 1.892 | 1.006 | **slower** |

Runs: base pg-win-Agr6xH, pg-win-2EjsNW; candidate pg-win-m6VGDw, pg-win-YBrHtI.

Measure a flagged case again alone and longer (--cases, --repetitions 101); if it stays, --copt=-falign-functions=64 tells whether it is the code's place.
