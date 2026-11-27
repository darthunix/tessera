# aggfold by cycles before the one-pass check: base 457b80d9, candidate d55788d2

### ab-aD05Dn

| case | base, M cycles | candidate, M cycles | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| fold_ten_int2 | 216.838 | 52.968 | 0.246 | 0.244 | 0.964 | faster |
| fold_two_int4 | 56.865 | 60.306 | 1.062 | 1.061 | 1.000 | **slower** |
| twin_ten_int2 | 56.078 | 52.896 | 0.945 | 0.943 | 0.995 | faster |
| twin_two_int4 | 44.568 | 41.836 | 0.946 | 0.939 | 0.995 | faster |

Runs: base pg-aggfold-U5vuU2, pg-aggfold-amizwK; candidate pg-aggfold-m2uY6L, pg-aggfold-Lg0AWK.

### ab-YYBKuJ

| case | base, M cycles | candidate, M cycles | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| fold_ten_int4 | 315.194 | 59.642 | 0.189 | 0.189 | 0.997 | faster |
| fold_ten_int8 | 243.578 | 58.487 | 0.246 | 0.240 | 1.004 | faster |
| twin_ten_int4 | 63.179 | 54.768 | 0.868 | 0.867 | 1.004 | faster |
| twin_ten_int8 | 58.283 | 53.579 | 0.914 | 0.919 | 1.000 | faster |

Runs: base pg-aggfold-3yrOtJ, pg-aggfold-n4qjt9; candidate pg-aggfold-ZlAjon, pg-aggfold-fOkvao.

### ab-ps26S5

| case | base, M cycles | candidate, M cycles | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| fold_kinds_int4 | 223.904 | 73.970 | 0.321 | 0.330 | 1.005 | faster |
| fold_ten_numeric | 545.939 | 83.589 | 0.154 | 0.153 | 0.990 | faster |
| twin_kinds_int4 | 63.067 | 54.881 | 0.873 | 0.870 | 0.993 | faster |
| twin_ten_numeric | 99.628 | 83.859 | 0.842 | 0.842 | 1.006 | faster |

Runs: base pg-aggfold-TdbGUp, pg-aggfold-ac7A5q; candidate pg-aggfold-s0Y3fm, pg-aggfold-WAgQPw.

### ab-APEsbh

| case | base, M cycles | candidate, M cycles | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| fold_ten_abs | 534.317 | 91.937 | 0.172 | 0.172 | 1.002 | faster |
| fold_ten_int4_k37 | 506.125 | 171.649 | 0.338 | 0.339 | 1.000 | faster |
| twin_ten_abs | 106.314 | 73.815 | 0.695 | 0.694 | 1.000 | faster |
| twin_ten_int4_k37 | 190.949 | 185.775 | 0.976 | 0.973 | 1.009 | faster |

Runs: base pg-aggfold-sxduMi, pg-aggfold-ZsmqkQ; candidate pg-aggfold-QDq3zn, pg-aggfold-EEZOj5.

### ab-vIowmr

| case | base, M cycles | candidate, M cycles | min | median | control | disk | |
|---|---:|---:|---:|---:|---:|---:|---|
| fold_ten_int4_m1m | 1274.993 | 784.354 | 0.618 | 0.615 | 1.005 | 0.181 | faster, less disk |
| twin_ten_int4_m1m | 652.183 | 651.703 | 1.003 | 0.999 | 0.998 | 1.000 |  |

Runs: base pg-aggfold-QACzh0, pg-aggfold-fuVZMG; candidate pg-aggfold-vHUWJU, pg-aggfold-wS6PLp.

### ab-HoDeJI

| case | base, M cycles | candidate, M cycles | min | median | control | disk | |
|---|---:|---:|---:|---:|---:|---:|---|
| fold_ten_numeric_m1m | 8206.936 | 2051.218 | – | – | 1.024 | 0.204 | **plan differs**, less disk |
| twin_ten_numeric_m1m | 1164.468 | 1083.481 | 0.918 | 0.930 | 1.027 | 0.556 | faster, less disk |

Runs: base pg-aggfold-SJSJUM, pg-aggfold-6eHrqw; candidate pg-aggfold-91GZq2, pg-aggfold-bqBw8r.

### ab-eMl1UN

| case | base, M cycles | candidate, M cycles | min | median | control | disk | |
|---|---:|---:|---:|---:|---:|---:|---|
| fold_distinct_int4_k37 | 1090.351 | 484.547 | 0.438 | 0.444 | 1.079 | 0.317 | faster, less disk |
| fold_mixed | 216.549 | 203.640 | 0.925 | 0.940 | 1.039 | – | faster |
| twin_distinct_int4_k37 | 461.981 | 466.452 | 1.002 | 1.010 | 1.079 | 1.000 |  |

Runs: base pg-aggfold-iNVrQS, pg-aggfold-uWTyDV; candidate pg-aggfold-IzNb9X, pg-aggfold-khZZOZ.

### ab-6vym5u

| case | base, M cycles | candidate, M cycles | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| guard_float | 475.217 | 473.241 | 0.994 | 0.996 | 0.999 |  |
| guard_lone | 47.803 | 48.338 | 1.020 | 1.011 | 1.004 |  |
| guard_parallel | 353.366 | 60.411 | – | – | 1.024 | **plan differs** |
| guard_tie | 57.372 | 52.089 | 0.908 | 0.908 | 0.993 | faster |

Runs: base pg-aggfold-9AiDA4, pg-aggfold-B4lARV; candidate pg-aggfold-GEcEWK, pg-aggfold-j7C6hQ.

### ab-rDcP6g

| case | base, M cycles | candidate, M cycles | min | median | control | disk | |
|---|---:|---:|---:|---:|---:|---:|---|
| fold_ten_int4_spill | 1266.739 | 776.382 | 0.613 | 0.613 | 0.989 | 0.181 | faster, less disk |
| twin_ten_int4_spill | 647.087 | 645.226 | 1.002 | 0.997 | 1.003 | 1.000 |  |

Runs: base pg-aggfold-3lMn0F, pg-aggfold-EC1YK1; candidate pg-aggfold-HCHv16, pg-aggfold-caZ5s7.

# The check of errors, two ways, by cycles

The kernels' min and max, two calls a batch:

### ab-9tNs31

| case | base, M cycles | candidate, M cycles | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| fold_kinds_int4 | 221.510 | 65.515 | 0.295 | 0.296 | 1.002 | faster |
| fold_ten_abs | 529.913 | 85.031 | 0.161 | 0.160 | 0.998 | faster |
| fold_two_int4 | 59.147 | 53.934 | 0.953 | 0.912 | 1.034 | faster |
| twin_kinds_int4 | 62.749 | 54.427 | 0.867 | 0.867 | 0.988 | faster |
| twin_ten_abs | 106.057 | 72.798 | 0.693 | 0.686 | 0.998 | faster |
| twin_two_int4 | 44.474 | 41.440 | 0.940 | 0.932 | 1.001 | faster |

Runs: base pg-aggfold-R7mBQr, pg-aggfold-dvVYVC; candidate pg-aggfold-zNI538, pg-aggfold-tAhdvu.

One pass a word, kept (da90e0c6):

### ab-uqma2A

| case | base, M cycles | candidate, M cycles | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| fold_kinds_int4 | 222.438 | 57.693 | 0.260 | 0.259 | 1.001 | faster |
| fold_ten_abs | 531.175 | 76.314 | 0.143 | 0.144 | 0.998 | faster |
| fold_two_int4 | 57.516 | 45.206 | 0.802 | 0.786 | 1.006 | faster |
| twin_kinds_int4 | 62.955 | 54.730 | 0.867 | 0.869 | 1.001 | faster |
| twin_ten_abs | 105.704 | 73.049 | 0.690 | 0.691 | 1.045 | faster |
| twin_two_int4 | 44.831 | 41.909 | 0.940 | 0.935 | 1.008 | faster |

Runs: base pg-aggfold-J8u60h, pg-aggfold-OQCCpB; candidate pg-aggfold-xfcRG3, pg-aggfold-8dTJWc.

