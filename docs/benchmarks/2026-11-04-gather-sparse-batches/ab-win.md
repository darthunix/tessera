# A/B: b04868b189bdb870f1df40b22986e6406c637e46 against 9c8b2953e2040c07e067ae6d2d247ab8852322ea

PostgreSQL 20 (/private/tmp/claude-501/-Users-darthunix-git-tessera/2dc6b822-803f-4d35-9c2a-a0ed4421e78f/scratchpad/pgmaster/bin/pg_config); 2 round(s) a side, base and candidate alternating; cases ^(sparse|half|sum_sparse|wide|mixed_text|group_filter|float_filter)$; repetitions 7; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's time over the base's; below 1 is faster. `min` compares the least time of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 2% by its minimum and by its median beyond the control.

## win

| case | base, ms | candidate, ms | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| float_filter | 13.448 | 13.405 | 0.996 | 0.997 | 1.001 |  |
| group_filter | 21.195 | 21.208 | 1.003 | 1.001 | 0.997 |  |
| half | 9.663 | 9.588 | 0.987 | 0.992 | 1.000 |  |
| mixed_text | 3.539 | 3.529 | 1.000 | 0.997 | 0.998 |  |
| sparse | 10.463 | 10.419 | 1.001 | 0.996 | 0.999 |  |
| sum_sparse | 11.082 | 10.733 | 0.967 | 0.969 | 1.004 | faster |
| wide | 3.692 | 3.623 | 0.987 | 0.982 | 0.997 |  |

Runs: base pg-win-niBbRh, pg-win-fgt05Y; candidate pg-win-0ltdWo, pg-win-CAUcrf.

