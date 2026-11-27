# aggfold by cycles, the final series: base 457b80d9, candidate da90e0c6

### ab-CKEQiw

| case | base, M cycles | candidate, M cycles | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| fold_ten_int2 | 212.086 | 52.624 | 0.249 | 0.248 | 1.007 | faster |
| fold_two_int4 | 55.942 | 45.134 | 0.806 | 0.807 | 1.007 | faster |
| twin_ten_int2 | 55.035 | 52.590 | 0.950 | 0.956 | 1.000 | faster |
| twin_two_int4 | 44.117 | 41.890 | 0.940 | 0.950 | 1.005 | faster |

Runs: base pg-aggfold-xNQZhp, pg-aggfold-1LNVIV; candidate pg-aggfold-dRpmot, pg-aggfold-Dd9Zxk.

### ab-I0UaE1

| case | base, M cycles | candidate, M cycles | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| fold_ten_int4 | 315.825 | 45.758 | 0.145 | 0.145 | 0.999 | faster |
| fold_ten_int8 | 237.622 | 42.318 | 0.182 | 0.178 | 1.013 | faster |
| twin_ten_int4 | 63.164 | 54.530 | 0.864 | 0.863 | 1.029 | faster |
| twin_ten_int8 | 57.975 | 53.269 | 0.930 | 0.919 | 1.017 | faster |

Runs: base pg-aggfold-ycfEca, pg-aggfold-nPAWyj; candidate pg-aggfold-a9S4j5, pg-aggfold-gbzTE5.

### ab-A4j6iy

| case | base, M cycles | candidate, M cycles | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| fold_kinds_int4 | 223.369 | 57.989 | 0.261 | 0.260 | 0.998 | faster |
| fold_ten_numeric | 539.301 | 85.027 | 0.157 | 0.158 | 1.005 | faster |
| twin_kinds_int4 | 62.981 | 54.745 | 0.874 | 0.869 | 0.980 | faster |
| twin_ten_numeric | 99.294 | 83.420 | 0.842 | 0.840 | 0.997 | faster |

Runs: base pg-aggfold-b2XLds, pg-aggfold-5D0Qtf; candidate pg-aggfold-r6hn9j, pg-aggfold-BOL9wt.

### ab-McFXUM

| case | base, M cycles | candidate, M cycles | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| fold_ten_abs | 525.255 | 76.457 | 0.144 | 0.146 | 1.004 | faster |
| fold_ten_int4_k37 | 500.884 | 152.092 | 0.300 | 0.304 | 1.000 | faster |
| twin_ten_abs | 106.107 | 73.069 | 0.692 | 0.689 | 1.012 | faster |
| twin_ten_int4_k37 | 190.172 | 180.516 | 0.949 | 0.949 | 0.997 | faster |

Runs: base pg-aggfold-7cKCbF, pg-aggfold-gJBDVl; candidate pg-aggfold-OwEqJn, pg-aggfold-yJRxv0.

### ab-SHv5vP

| case | base, M cycles | candidate, M cycles | min | median | control | disk | |
|---|---:|---:|---:|---:|---:|---:|---|
| fold_ten_int4_m1m | 1271.840 | 768.236 | 0.607 | 0.604 | 1.002 | 0.181 | faster, less disk |
| twin_ten_int4_m1m | 649.692 | 653.002 | 1.009 | 1.005 | 1.005 | 1.000 |  |

Runs: base pg-aggfold-CAgqTC, pg-aggfold-XN6Ixk; candidate pg-aggfold-nc17zg, pg-aggfold-ODZ0lQ.

### ab-u6Leat

| case | base, M cycles | candidate, M cycles | min | median | control | disk | |
|---|---:|---:|---:|---:|---:|---:|---|
| fold_ten_numeric_m1m | 8214.685 | 1998.689 | – | – | 0.996 | 0.204 | **plan differs**, less disk |
| twin_ten_numeric_m1m | 1152.668 | 1059.920 | 0.918 | 0.920 | 0.992 | 0.556 | faster, less disk |

Runs: base pg-aggfold-X7Gfqr, pg-aggfold-VcCgWT; candidate pg-aggfold-Hj66p4, pg-aggfold-S7v2zI.

### ab-WtZMtH

| case | base, M cycles | candidate, M cycles | min | median | control | disk | |
|---|---:|---:|---:|---:|---:|---:|---|
| fold_distinct_int4_k37 | 1122.501 | 459.142 | 0.419 | 0.409 | 1.008 | 0.317 | faster, less disk |
| fold_mixed | 216.312 | 186.734 | 0.863 | 0.863 | 1.024 | – | faster |
| twin_distinct_int4_k37 | 461.910 | 471.944 | 0.986 | 1.022 | 1.004 | 1.000 |  |

Runs: base pg-aggfold-K3LEGG, pg-aggfold-ANXdGW; candidate pg-aggfold-YVEBai, pg-aggfold-hYPK87.

### ab-NvvzzF

| case | base, M cycles | candidate, M cycles | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| guard_float | 457.903 | 466.050 | 1.016 | 1.018 | 0.996 | **slower** |
| guard_lone | 47.631 | 48.043 | 1.008 | 1.009 | 1.018 |  |
| guard_parallel | 349.745 | 44.475 | – | – | 0.974 | **plan differs** |
| guard_tie | 56.952 | 52.815 | 0.912 | 0.927 | 1.006 | faster |

Runs: base pg-aggfold-B12kvs, pg-aggfold-Fybu8Q; candidate pg-aggfold-VbXlXh, pg-aggfold-wI6948.

### ab-wxAwyZ

| case | base, M cycles | candidate, M cycles | min | median | control | disk | |
|---|---:|---:|---:|---:|---:|---:|---|
| fold_ten_int4_spill | 1269.953 | 760.436 | 0.600 | 0.599 | 1.003 | 0.181 | faster, less disk |
| twin_ten_int4_spill | 643.668 | 643.954 | 1.000 | 1.000 | 1.003 | 1.000 |  |

Runs: base pg-aggfold-ANbDTr, pg-aggfold-jOTd9I; candidate pg-aggfold-OHOvPe, pg-aggfold-zlRBjG.

### ab-BLHenI

| case | base, M cycles | candidate, M cycles | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| guard_float | 459.753 | 468.278 | 1.003 | 1.019 | 1.003 |  |

Runs: base pg-aggfold-Ha6Xjg, pg-aggfold-rszFrX; candidate pg-aggfold-7nTqVX, pg-aggfold-GzDrPK.

