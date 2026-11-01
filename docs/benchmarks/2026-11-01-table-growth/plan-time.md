# A/B: a09534753517dc7e526d9001b2dabb96b653089c against fd35c5af2fe585bfd1a97895af23e43ed755453c

PostgreSQL 20 (a copy of the release build of master 0497318e442); 2 round(s) a side, base and candidate alternating; cases ^plan_time$; repetitions 101; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's time over the base's; below 1 is faster. `min` compares the least time of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 2% by its minimum and by its median beyond the control.

## join

| case | base, ms | candidate, ms | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| plan_time | 0.071 | 0.071 | 1.021 | 1.006 | 1.002 |  |

Runs: base pg-join-GWPsd3, pg-join-q6pLeG; candidate pg-join-t4I5F4, pg-join-YM5QoC.

