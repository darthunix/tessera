# A/B: 46a8957f22dea0552ff0ccc5ae257e733c09430b against 06297a9f265280e681b09129548bf61e133cfb71

PostgreSQL 20 (/private/tmp/claude-501/-Users-darthunix-git-tessera/2dc6b822-803f-4d35-9c2a-a0ed4421e78f/scratchpad/pgmaster/bin/pg_config); 2 round(s) a side, base and candidate alternating; cases ^(wide|mixed|mixed_text)$; repetitions 7; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's time over the base's; below 1 is faster. `min` compares the least time of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 2% by its minimum and by its median beyond the control.

## win

| case | base, ms | candidate, ms | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| mixed | 40.246 | 40.416 | 0.992 | 1.004 | 1.012 |  |
| mixed_text | 35.552 | 36.346 | 1.022 | 1.022 | 0.998 | **slower** |
| wide | 47.871 | 47.937 | 0.995 | 1.001 | 1.001 |  |

Runs: base pg-win-PKYP8T, pg-win-ZXqDU7; candidate pg-win-PKnIpO, pg-win-DfZPld.

Measure a flagged case again alone and longer (--cases, --repetitions 101); if it stays, --copt=-falign-functions=64 tells whether it is the code's place.

# A/B: 46a8957f22dea0552ff0ccc5ae257e733c09430b against 06297a9f265280e681b09129548bf61e133cfb71

PostgreSQL 20 (/private/tmp/claude-501/-Users-darthunix-git-tessera/2dc6b822-803f-4d35-9c2a-a0ed4421e78f/scratchpad/pgmaster/bin/pg_config); 3 round(s) a side, base and candidate alternating; cases ^mixed_text$; repetitions 31; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's time over the base's; below 1 is faster. `min` compares the least time of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 2% by its minimum and by its median beyond the control.

## win

| case | base, ms | candidate, ms | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| mixed_text | 37.458 | 35.132 | 1.011 | 0.938 | 1.004 |  |

Runs: base pg-win-XXEzMr, pg-win-SamWTg, pg-win-0VnCyo; candidate pg-win-NgmyN7, pg-win-rBqCmF, pg-win-4J55rw.

