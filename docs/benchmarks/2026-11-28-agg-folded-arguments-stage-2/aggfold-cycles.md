# aggfold by cycles, two rounds: base a71b632a, candidate 68e233db

### ab-PAv70C

| case | base, M cycles | candidate, M cycles | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| fold_ten_int2 | 52.455 | 52.473 | 0.998 | 1.000 | 0.997 |  |
| fold_two_int4 | 45.232 | 45.235 | 0.997 | 1.000 | 0.995 |  |
| twin_ten_int2 | 52.473 | 52.652 | 1.005 | 1.003 | 0.999 |  |
| twin_two_int4 | 41.532 | 41.148 | 0.992 | 0.991 | 0.998 |  |

Runs: base pg-aggfold-nxE0P9, pg-aggfold-3pNncH; candidate pg-aggfold-OPuo5w, pg-aggfold-1P3R59.

### ab-ggN8N5

| case | base, M cycles | candidate, M cycles | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| fold_ten_int4 | 45.476 | 44.862 | 0.988 | 0.986 | 0.995 |  |
| fold_ten_int8 | 42.079 | 42.277 | 1.009 | 1.005 | 0.995 |  |
| twin_ten_int4 | 54.590 | 54.445 | 0.993 | 0.997 | 0.987 |  |
| twin_ten_int8 | 53.192 | 52.990 | 0.992 | 0.996 | 0.995 |  |

Runs: base pg-aggfold-sJv6wI, pg-aggfold-YGTg9o; candidate pg-aggfold-ngBT6K, pg-aggfold-rh3Oi9.

### ab-O3mAjD

| case | base, M cycles | candidate, M cycles | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| fold_kinds_int4 | 57.110 | 57.557 | 1.013 | 1.008 | 1.003 |  |
| fold_ten_numeric | 84.646 | 84.945 | 1.013 | 1.004 | 1.008 |  |
| twin_kinds_int4 | 53.961 | 54.258 | 1.002 | 1.006 | 0.999 |  |
| twin_ten_numeric | 83.901 | 84.650 | 1.012 | 1.009 | 1.007 |  |

Runs: base pg-aggfold-vVgTn8, pg-aggfold-UwY5Zp; candidate pg-aggfold-ecbhdP, pg-aggfold-cQ8ejw.

### ab-gngfOX

| case | base, M cycles | candidate, M cycles | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| fold_ten_abs | 76.011 | 76.289 | 1.002 | 1.004 | 0.995 |  |
| fold_ten_int4_k37 | 153.170 | 150.483 | 0.981 | 0.982 | 1.004 | faster |
| twin_ten_abs | 73.357 | 73.139 | 0.994 | 0.997 | 0.990 |  |
| twin_ten_int4_k37 | 180.266 | 177.395 | 0.990 | 0.984 | 0.991 |  |

Runs: base pg-aggfold-9qIHnw, pg-aggfold-Up3fP4; candidate pg-aggfold-qpyMCA, pg-aggfold-LxEWxg.

### ab-p11ja6

| case | base, M cycles | candidate, M cycles | min | median | control | disk | |
|---|---:|---:|---:|---:|---:|---:|---|
| fold_ten_int4_m1m | 765.622 | 771.773 | 1.000 | 1.008 | 0.994 | 1.000 |  |
| twin_ten_int4_m1m | 648.695 | 658.076 | 1.006 | 1.014 | 1.000 | 1.000 |  |

Runs: base pg-aggfold-TCxIDb, pg-aggfold-PtQvoe; candidate pg-aggfold-qyVQoV, pg-aggfold-ZzcRNA.

### ab-VWpz9e

| case | base, M cycles | candidate, M cycles | min | median | control | disk | |
|---|---:|---:|---:|---:|---:|---:|---|
| fold_ten_numeric_m1m | 2074.429 | 2385.863 | 1.005 | 1.150 | 1.076 | 1.000 |  |
| twin_ten_numeric_m1m | 2664.313 | 2896.706 | 1.027 | 1.087 | 1.079 | 1.000 |  |

Runs: base pg-aggfold-rNT0QP, pg-aggfold-3ZR77N; candidate pg-aggfold-mYCn42, pg-aggfold-Fdyp57.

### ab-GbFHju

| case | base, M cycles | candidate, M cycles | min | median | control | disk | |
|---|---:|---:|---:|---:|---:|---:|---|
| fold_distinct_int4_k37 | 459.007 | 459.944 | 1.011 | 1.002 | 1.000 | 1.000 |  |
| fold_mixed | 182.132 | 184.820 | 1.011 | 1.015 | 0.990 | – | **slower** |
| twin_distinct_int4_k37 | 450.335 | 454.350 | 1.008 | 1.009 | 1.000 | 1.000 |  |

Runs: base pg-aggfold-ZuvQ5Y, pg-aggfold-4cvMLx; candidate pg-aggfold-f9VTnh, pg-aggfold-euTziq.

### ab-69rhDa

| case | base, M cycles | candidate, M cycles | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| guard_float | 476.920 | 476.475 | 1.004 | 0.999 | 1.002 |  |
| guard_lone | 49.061 | 48.250 | 0.986 | 0.983 | 1.003 | faster |
| guard_parallel | 46.302 | 44.944 | 0.988 | 0.971 | 0.969 |  |
| guard_tie | 53.299 | 52.041 | 0.987 | 0.976 | 1.019 | faster |

Runs: base pg-aggfold-7TJBMc, pg-aggfold-KLCEba; candidate pg-aggfold-Bt7ori, pg-aggfold-JOWuXf.

### ab-XuYDUz

| case | base, M cycles | candidate, M cycles | min | median | control | disk | |
|---|---:|---:|---:|---:|---:|---:|---|
| fold_ten_int4_spill | 794.523 | 783.919 | 1.006 | 0.987 | 1.018 | 1.000 |  |
| twin_ten_int4_spill | 696.997 | 672.481 | 0.999 | 0.965 | 0.999 | 1.000 |  |

Runs: base pg-aggfold-weprjx, pg-aggfold-F9pKxX; candidate pg-aggfold-B8TDmS, pg-aggfold-mpVgl2.

## The third pair of the numeric cases of a million groups

### ab-Weleyg

| case | base, M cycles | candidate, M cycles | min | median | control | disk | |
|---|---:|---:|---:|---:|---:|---:|---|
| fold_ten_numeric_m1m | 2082.584 | 2028.464 | 0.982 | 0.974 | 0.992 | 1.000 | faster |
| twin_ten_numeric_m1m | 2654.097 | 2695.969 | 0.991 | 1.016 | 0.999 | 1.000 |  |

Runs: base pg-aggfold-Kctz6m; candidate pg-aggfold-iivLqZ.
