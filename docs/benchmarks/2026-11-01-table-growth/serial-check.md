# A/B: 55529005957e90d3b9ea907a8c39eefba6e83c96 against 6e7377d54162ac3088924e8339de88017516d40a

PostgreSQL 20 (a copy of the release build of master 0497318e442); 2 round(s) a side, base and candidate alternating; cases ^build_(250k|300k|420k)$; repetitions 11; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's time over the base's; below 1 is faster. `min` compares the least time of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 2% by its minimum and by its median beyond the control.

## spill

| case | base, ms | candidate, ms | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| build_250k | 34.599 | 31.375 | 0.975 | 0.907 | 0.953 | faster |
| build_300k | 31.018 | 30.492 | 1.012 | 0.983 | 0.889 |  |
| build_420k | 38.372 | 49.691 | 1.364 | 1.295 | 0.951 | **slower** |

Runs: base pg-spill-6wyCD0, pg-spill-mGQppa; candidate pg-spill-aFr0Gf, pg-spill-RbvHah.

Measure a flagged case again alone and longer (--cases, --repetitions 101); if it stays, --copt=-falign-functions=64 tells whether it is the code's place.
