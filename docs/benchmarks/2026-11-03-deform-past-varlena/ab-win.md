# A/B: 46a8957f22dea0552ff0ccc5ae257e733c09430b against 06297a9f265280e681b09129548bf61e133cfb71

PostgreSQL 20 (a release build of master); 2 round(s) a side, base and candidate alternating; cases all; repetitions 7; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's time over the base's; below 1 is faster. `min` compares the least time of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 2% by its minimum and by its median beyond the control.

## win

| case | base, ms | candidate, ms | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| case_filter | 17.125 | 16.617 | 1.012 | 0.970 | 0.975 |  |
| case_sum | 16.263 | 16.146 | 1.000 | 0.993 | 0.907 |  |
| count_all | 5.574 | 5.540 | 1.022 | 0.994 | 0.958 | **slower** |
| date_ts_filter | 8.254 | 8.264 | 1.006 | 1.001 | 0.939 |  |
| dense | 11.854 | 11.506 | 1.020 | 0.971 | 0.990 |  |
| either | 12.059 | 12.241 | 0.993 | 1.015 | 0.957 |  |
| expr | 13.154 | 13.080 | 1.009 | 0.994 | 0.998 |  |
| float4_mixed | 21.549 | 21.129 | 1.012 | 0.981 | 0.972 |  |
| float_filter | 13.967 | 13.765 | 1.009 | 0.986 | 0.974 |  |
| float_sum | 17.157 | 16.677 | 1.020 | 0.972 | 0.986 |  |
| four_dense | 16.767 | 16.729 | 1.007 | 0.998 | 0.957 |  |
| group_few | 30.575 | 29.902 | 1.005 | 0.978 | 0.988 |  |
| group_filter | 22.444 | 21.748 | 0.999 | 0.969 | 0.987 |  |
| group_many | 37.248 | 35.472 | 0.996 | 0.952 | 0.984 |  |
| half | 9.810 | 9.951 | 1.019 | 1.014 | 1.003 |  |
| in_list | 11.902 | 11.952 | 1.007 | 1.004 | 0.988 |  |
| int2_arith | 22.712 | 22.138 | 0.998 | 0.975 | 0.998 |  |
| int2_div | 20.542 | 20.478 | 0.997 | 0.997 | 0.969 |  |
| int4_bigint | 10.361 | 10.028 | 1.009 | 0.968 | 0.955 |  |
| mixed | 3.905 | 3.831 | 1.003 | 0.981 | 1.017 |  |
| mixed_text | 3.674 | 3.598 | 1.014 | 0.979 | 0.999 |  |
| nothing | 7.941 | 8.180 | 1.019 | 1.030 | 0.906 |  |
| residual | 10.696 | 10.461 | 1.008 | 0.978 | 1.006 |  |
| sparse | 10.717 | 10.639 | 1.007 | 0.993 | 0.963 |  |
| sum_sparse | 11.585 | 11.412 | 1.008 | 0.985 | 0.988 |  |
| tree | 22.633 | 22.440 | 1.021 | 0.991 | 1.004 |  |
| tree_sum | 22.663 | 22.448 | 1.002 | 0.991 | 1.041 |  |
| tstz_cast | 82.811 | 83.220 | 1.014 | 1.005 | 0.993 |  |
| tstz_spans | 99.084 | 97.501 | 0.976 | 0.984 | 0.983 |  |
| tstz_trunc | 76.351 | 76.462 | 0.999 | 1.001 | 1.001 |  |
| two_columns | 11.717 | 11.599 | 1.007 | 0.990 | 0.996 |  |
| wide | 4.095 | 3.759 | 0.950 | 0.918 | 0.873 |  |

Runs: base pg-win-3QpJp9, pg-win-FGPrkA; candidate pg-win-q6pukF, pg-win-OX2CiX.

Measure a flagged case again alone and longer (--cases, --repetitions 101); if it stays, --copt=-falign-functions=64 tells whether it is the code's place.
