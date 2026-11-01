# A/B: WORKTREE@3c225f312018cc3842d47c88f9ddb9858392958c against 081173b18f0bb835744ee2ebe4dbe337c1acb4d0

PostgreSQL 20 (/Users/darthunix/git/postgres-review/build-release/bin/pg_config); 2 round(s) a side, base and candidate alternating; cases ^(rows_text|runs_text)$; repetitions the family's; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's time over the base's; below 1 is faster. `min` compares the least time of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 2% by its minimum and by its median beyond the control.

## exec

| case | base, ms | candidate, ms | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| rows_text | 45.574 | 48.313 | 1.083 | 1.060 | 0.951 | **slower** |
| runs_text | 125.704 | 124.648 | 1.005 | 0.992 | 0.986 |  |

Runs: base pg-exec-W1xL8z, pg-exec-mBfLkg; candidate pg-exec-UJt2Xf, pg-exec-Lg4GmL.

Measure a flagged case again alone and longer (--cases, --repetitions 101); if it stays, --copt=-falign-functions=64 tells whether it is the code's place.
