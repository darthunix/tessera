# A/B: 5220f5090729f1429e9f5dfdcd15d1be70e1a218 against 481911ee83200af65e350ae05c36e9290ce00262

PostgreSQL 20 (a release build of master); 2 round(s) a side, base and candidate alternating; cases all; repetitions the family's; workers 0; COPT none; Now drawing from 'AC Power'.

Each ratio is the candidate's time over the base's; below 1 is faster. `min` compares the least time of any run, `median` the mean of the runs' medians, `control` the medians of mode off, without Tessera, which move with the machine, not with the change. A case is flagged when it moves past 2% by its minimum and by its median beyond the control.

## indexjoin

| case | base, ms | candidate, ms | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| anti_both_sides | 6.418 | 1.926 | 0.301 | 0.300 | 1.000 | faster |
| anti_fifth | 5.693 | 1.643 | 0.296 | 0.289 | 1.000 | faster |
| anti_miss | 2.256 | 2.268 | 0.995 | 1.005 | 1.014 |  |
| anti_none | 4.107 | 1.540 | 0.371 | 0.375 | 1.006 | faster |
| cross_type | 1.691 | 1.715 | 1.022 | 1.014 | 1.000 |  |
| date_key | 1.748 | 1.737 | 1.008 | 0.993 | 1.013 |  |
| few | 0.871 | 0.868 | 1.005 | 0.997 | 0.993 |  |
| four | 1.144 | 1.145 | 0.992 | 1.000 | 0.998 |  |
| hot_key | 224.922 | 224.178 | 1.000 | 0.997 | 1.019 |  |
| left_miss | 2.418 | 2.427 | 0.997 | 1.004 | 1.006 |  |
| pk | 1.742 | 1.728 | 0.991 | 0.991 | 0.997 |  |
| pk_20k | 4.819 | 4.660 | 0.974 | 0.967 | 1.000 | faster |
| pk_shuffled | 6.095 | 5.740 | 0.994 | 0.942 | 0.989 |  |
| repeated | 2.633 | 2.717 | 1.007 | 1.032 | 1.000 |  |
| scattered | 2.498 | 2.414 | 0.999 | 0.966 | 0.966 |  |
| semi_all | 2.410 | 2.433 | 1.006 | 1.009 | 1.002 |  |
| semi_fifth | 7.684 | 1.623 | 0.214 | 0.211 | 1.001 | faster |
| semi_pk | 1.598 | 1.644 | 1.014 | 1.029 | 1.001 |  |
| semi_shuffled | 4.723 | 4.861 | 1.058 | 1.029 | 0.949 | **slower** |
| semi_three_fifths | 2.533 | 1.718 | 0.671 | 0.678 | 1.005 | faster |
| sparse | 3.514 | 3.585 | 0.935 | 1.020 | 1.057 | faster |
| text_key | 5.691 | 5.769 | 0.995 | 1.014 | 1.039 |  |

Runs: base pg-indexjoin-uL0bP7, pg-indexjoin-Ro0dbr; candidate pg-indexjoin-GxcWba, pg-indexjoin-N1u5id.

Measure a flagged case again alone and longer (--cases, --repetitions 101); if it stays, --copt=-falign-functions=64 tells whether it is the code's place.
