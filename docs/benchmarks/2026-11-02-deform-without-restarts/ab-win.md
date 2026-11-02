# A/B: c3b22e23e78f91b2477e37adb69b5c1e1ab2e96f against 74d7e39823d37b3c66dbcede920236a4b0e2164d

PostgreSQL 20 (a release build of master); 2 round(s) a side, base and candidate alternating; cases all; repetitions 21; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's time over the base's; below 1 is faster. `min` compares the least time of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 2% by its minimum and by its median beyond the control.

## win

| case | base, ms | candidate, ms | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| case_filter | 15.565 | 16.477 | 1.002 | 1.059 | 1.017 |  |
| case_sum | 15.723 | 16.710 | 1.003 | 1.063 | 1.043 |  |
| count_all | 5.517 | 5.674 | 0.995 | 1.028 | 1.019 |  |
| dense | 11.252 | 11.893 | 0.997 | 1.057 | 1.042 |  |
| either | 11.759 | 12.804 | 1.009 | 1.089 | 1.039 |  |
| expr | 12.817 | 12.870 | 0.993 | 1.004 | 1.021 |  |
| float4_mixed | 20.716 | 21.441 | 1.006 | 1.035 | 1.034 |  |
| float_filter | 13.534 | 14.364 | 1.005 | 1.061 | 1.051 |  |
| float_sum | 16.218 | 16.881 | 1.017 | 1.041 | 1.029 |  |
| four_dense | 15.785 | 17.380 | 1.003 | 1.101 | 1.065 |  |
| group_few | 29.215 | 29.954 | 1.000 | 1.025 | 1.104 |  |
| group_filter | 20.986 | 21.574 | 1.010 | 1.028 | 1.028 |  |
| group_many | 34.619 | 35.337 | 0.998 | 1.021 | 1.019 |  |
| half | 9.866 | 10.236 | 0.995 | 1.038 | 1.035 |  |
| in_list | 11.490 | 11.899 | 0.988 | 1.036 | 1.147 |  |
| int4_bigint | 9.597 | 10.375 | 1.014 | 1.081 | 1.056 |  |
| mixed | 3.754 | 3.834 | 0.970 | 1.021 | 1.032 |  |
| mixed_text | 3.508 | 3.423 | 1.011 | 0.976 | 1.025 |  |
| nothing | 7.919 | 8.659 | 1.000 | 1.093 | 1.027 |  |
| residual | 10.534 | 10.966 | 0.993 | 1.041 | 1.024 |  |
| sparse | 10.382 | 10.741 | 0.983 | 1.035 | 1.033 |  |
| sum_sparse | 11.378 | 11.343 | 0.999 | 0.997 | 1.020 |  |
| tree | 21.027 | 21.679 | 0.994 | 1.031 | 1.026 |  |
| tree_sum | 21.053 | 21.540 | 1.002 | 1.023 | 0.987 |  |
| two_columns | 11.195 | 11.788 | 1.002 | 1.053 | 1.031 |  |
| wide | 3.867 | 4.115 | 0.995 | 1.064 | 1.049 |  |

Runs: base pg-win-aSHDkv, pg-win-rSFjAK; candidate pg-win-p3ultf, pg-win-IIWOAo.

