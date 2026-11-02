# A/B: a168afe93f6828e469639909ed0d53b64b1820f7 against c14180a55349d8f424f503dd74766c276b9b01be

PostgreSQL 20 (a copy of the release build of master 0497318e442); 2 round(s) a side, base and candidate alternating; cases ^(int2|date_ts|tstz); repetitions 21; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's time over the base's; below 1 is faster. `min` compares the least time of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 2% by its minimum and by its median beyond the control.

## win

| case | base, ms | candidate, ms | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| date_ts_filter | 8.297 | 7.997 | 0.981 | 0.964 | 0.999 |  |
| int2_arith | 21.638 | 21.402 | 0.998 | 0.989 | 0.995 |  |
| int2_div | 19.812 | 19.880 | 1.007 | 1.003 | 0.997 |  |
| tstz_cast | 81.133 | 136.260 | 1.711 | 1.679 | 1.026 | **slower** |
| tstz_spans | 101.072 | 249.865 | 2.399 | 2.472 | 1.032 | **slower** |
| tstz_trunc | 74.852 | 133.197 | 1.805 | 1.779 | 1.003 | **slower** |

Runs: base pg-win-MWY5lX, pg-win-l9tuAx; candidate pg-win-ma7zcS, pg-win-tdSSTy.

Measure a flagged case again alone and longer (--cases, --repetitions 101); if it stays, --copt=-falign-functions=64 tells whether it is the code's place.
