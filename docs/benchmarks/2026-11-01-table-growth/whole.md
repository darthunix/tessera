# A/B: a09534753517dc7e526d9001b2dabb96b653089c against fd35c5af2fe585bfd1a97895af23e43ed755453c

PostgreSQL 20 (a copy of the release build of master 0497318e442); 2 round(s) a side, base and candidate alternating; cases ^(fk_count|dup|left_dup|build_250k|build_300k|build_420k|group_fk_1536kb|distinct_128kb|regrow_128kb)$; repetitions 11; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's time over the base's; below 1 is faster. `min` compares the least time of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 2% by its minimum and by its median beyond the control.

## join

| case | base, ms | candidate, ms | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| dup | 34.303 | 33.566 | 0.985 | 0.978 | 1.001 |  |
| fk_count | 22.771 | 22.422 | 1.002 | 0.985 | 1.003 |  |
| left_dup | 2.733 | 2.738 | 1.012 | 1.002 | 1.036 |  |
| plan_time | 0.071 | 0.076 | 1.030 | 1.067 | 1.043 | **slower** |

Runs: base pg-join-3280qQ, pg-join-JxifAA; candidate pg-join-vm4rMV, pg-join-MH22j4.

## spill

| case | base, ms | candidate, ms | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| build_250k | 37.191 | 30.835 | 0.993 | 0.829 | 0.656 |  |
| build_300k | 28.285 | 46.346 | 1.621 | 1.639 | 0.934 | **slower** |
| build_420k | 35.240 | 59.438 | 1.730 | 1.687 | 0.992 | **slower** |
| distinct_128kb | 42.667 | 41.668 | 0.990 | 0.977 | 1.001 |  |
| group_fk_1536kb | 85.347 | 88.106 | 1.004 | 1.032 | 0.946 |  |
| regrow_128kb | 167.731 | 166.749 | 1.006 | 0.994 | 0.941 |  |

Runs: base pg-spill-hwK4aY, pg-spill-JaUDbl; candidate pg-spill-4FA3hK, pg-spill-ySsk0r.

Measure a flagged case again alone and longer (--cases, --repetitions 101); if it stays, --copt=-falign-functions=64 tells whether it is the code's place.
