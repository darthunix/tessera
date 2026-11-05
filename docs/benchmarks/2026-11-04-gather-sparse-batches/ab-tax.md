# A/B: 37cfba07aacad748e54911d86a77d7ee0bf68ff9 against 9c8b2953e2040c07e067ae6d2d247ab8852322ea

PostgreSQL 20 (/private/tmp/claude-501/-Users-darthunix-git-tessera/2dc6b822-803f-4d35-9c2a-a0ed4421e78f/scratchpad/pgmaster/bin/pg_config); 2 round(s) a side, base and candidate alternating; cases .; repetitions 7; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's time over the base's; below 1 is faster. `min` compares the least time of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 2% by its minimum and by its median beyond the control.

## tax

| case | base, ms | candidate, ms | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| limit_1 | 0.008 | 0.008 | 1.250 | 1.000 | 1.000 |  |
| limit_1000 | 0.010 | 0.009 | 1.000 | 0.950 | 1.000 |  |
| limit_1m | 3.074 | 3.111 | 1.026 | 1.012 | 1.000 |  |
| limit_expr | 11.134 | 11.375 | 1.005 | 1.022 | 1.017 |  |
| limit_text | 2.965 | 3.002 | 1.011 | 1.013 | 0.997 |  |
| mixed_limit | 3.021 | 3.024 | 1.002 | 1.001 | 1.006 |  |
| offset_1m | 2.586 | 2.595 | 1.008 | 1.003 | 0.996 |  |
| sort_limit | 36.540 | 35.791 | 0.986 | 0.980 | 0.984 |  |
| wide_limit | 1.209 | 1.236 | 1.028 | 1.023 | 1.015 |  |

Runs: base pg-tax-SrMDZ4, pg-tax-k0XHmE; candidate pg-tax-u5HmPe, pg-tax-ZVJAWh.

