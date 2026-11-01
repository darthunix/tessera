# A/B: a09534753517dc7e526d9001b2dabb96b653089c against fd35c5af2fe585bfd1a97895af23e43ed755453c

PostgreSQL 20 (a copy of the release build of master 0497318e442); 2 round(s) a side, base and candidate alternating; cases ^mixed_text_16mb$; repetitions 51; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's time over the base's; below 1 is faster. `min` compares the least time of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 2% by its minimum and by its median beyond the control.

## spill

| case | base, ms | candidate, ms | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| mixed_text_16mb | 63.415 | 84.511 | 1.371 | 1.333 | 0.903 | **slower** |

Runs: base pg-spill-mDHsy9, pg-spill-nRJ5ka; candidate pg-spill-qhobjg, pg-spill-w4D5Vn.

Measure a flagged case again alone and longer (--cases, --repetitions 101); if it stays, --copt=-falign-functions=64 tells whether it is the code's place.
