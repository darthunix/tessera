# A/B: 9356c3fb80a5f3246b769dc20e6589a77472e7e0 against c14180a55349d8f424f503dd74766c276b9b01be

PostgreSQL 20 (a copy of the release build of master 0497318e442); 2 round(s) a side, base and candidate alternating; cases ^(int2|date_ts|tstz); repetitions 21; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's time over the base's; below 1 is faster. `min` compares the least time of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 2% by its minimum and by its median beyond the control.

## win

| case | base, ms | candidate, ms | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| date_ts_filter | 8.140 | 8.967 | 1.021 | 1.102 | 1.033 | **slower** |
| int2_arith | 22.823 | 22.977 | 0.977 | 1.007 | 1.016 |  |
| int2_div | 20.520 | 20.913 | 0.994 | 1.019 | 1.022 |  |
| tstz_cast | 85.579 | 84.549 | 0.984 | 0.988 | 1.034 |  |
| tstz_spans | 108.569 | 100.897 | 0.920 | 0.929 | 0.915 |  |
| tstz_trunc | 78.444 | 78.488 | 1.019 | 1.001 | 1.004 |  |

Runs: base pg-win-JWvO0Y, pg-win-KAAm30; candidate pg-win-01ew9u, pg-win-4w6zvu.

Measure a flagged case again alone and longer (--cases, --repetitions 101); if it stays, --copt=-falign-functions=64 tells whether it is the code's place.
