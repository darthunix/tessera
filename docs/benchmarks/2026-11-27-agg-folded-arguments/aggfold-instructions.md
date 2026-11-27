# aggfold by instructions: base 457b80d9, candidate da90e0c6

### ab-z8d2nj

| case | base, M instructions | candidate, M instructions | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| fold_ten_int2 | 1154.539 | 291.947 | 0.253 | 0.253 | 1.000 | faster |
| fold_two_int4 | 313.003 | 261.394 | 0.835 | 0.835 | 1.000 | faster |
| twin_ten_int2 | 306.347 | 291.934 | 0.953 | 0.953 | 1.000 | faster |
| twin_two_int4 | 255.234 | 241.039 | 0.944 | 0.944 | 1.000 | faster |

Runs: base pg-aggfold-SK7xv7; candidate pg-aggfold-7feGM6.

### ab-bb7DtM

| case | base, M instructions | candidate, M instructions | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| fold_ten_int4 | 1709.628 | 261.477 | 0.153 | 0.153 | 1.000 | faster |
| fold_ten_int8 | 1499.880 | 269.169 | 0.179 | 0.179 | 1.000 | faster |
| twin_ten_int4 | 345.589 | 303.055 | 0.877 | 0.877 | 1.000 | faster |
| twin_ten_int8 | 343.172 | 314.785 | 0.917 | 0.917 | 1.000 | faster |

Runs: base pg-aggfold-UT5Ega; candidate pg-aggfold-c4iEMG.

### ab-QDFuDZ

| case | base, M instructions | candidate, M instructions | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| fold_kinds_int4 | 1182.039 | 323.444 | 0.274 | 0.274 | 1.000 | faster |
| fold_ten_numeric | 4013.469 | 638.632 | 0.159 | 0.159 | 1.000 | faster |
| twin_kinds_int4 | 345.592 | 303.059 | 0.877 | 0.877 | 1.000 | faster |
| twin_ten_numeric | 710.991 | 638.639 | 0.898 | 0.898 | 1.000 | faster |

Runs: base pg-aggfold-YaiBUo; candidate pg-aggfold-HANZE1.

### ab-LJmQ4P

| case | base, M instructions | candidate, M instructions | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| fold_ten_abs | 2726.042 | 418.943 | 0.154 | 0.154 | 1.000 | faster |
| fold_ten_int4_k37 | 3080.667 | 1004.551 | 0.326 | 0.326 | 1.000 | faster |
| twin_ten_abs | 570.203 | 398.770 | 0.699 | 0.699 | 1.000 | faster |
| twin_ten_int4_k37 | 1253.326 | 1210.848 | 0.966 | 0.966 | 1.000 | faster |

Runs: base pg-aggfold-8xPJN6; candidate pg-aggfold-H18QGj.

### ab-CSzqYb

| case | base, M instructions | candidate, M instructions | min | median | control | disk | |
|---|---:|---:|---:|---:|---:|---:|---|
| fold_ten_int4_m1m | 6765.195 | 4677.247 | 0.691 | 0.691 | 1.000 | 0.181 | faster, less disk |
| twin_ten_int4_m1m | 3416.285 | 3450.056 | 1.010 | 1.010 | 1.001 | 1.000 |  |

Runs: base pg-aggfold-IZFD6a; candidate pg-aggfold-nelIQs.

### ab-rA1diw

| case | base, M instructions | candidate, M instructions | min | median | control | disk | |
|---|---:|---:|---:|---:|---:|---:|---|
| fold_ten_numeric_m1m | 53726.988 | 13742.884 | – | – | 1.000 | 0.204 | **plan differs**, less disk |
| twin_ten_numeric_m1m | 6911.591 | 6475.473 | 0.937 | 0.937 | 1.000 | 0.556 | faster, less disk |

Runs: base pg-aggfold-KyKExa; candidate pg-aggfold-RZUT5F.

### ab-VO5Nkl

| case | base, M instructions | candidate, M instructions | min | median | control | disk | |
|---|---:|---:|---:|---:|---:|---:|---|
| fold_distinct_int4_k37 | 4734.992 | 2174.304 | 0.459 | 0.459 | 1.002 | 0.317 | faster, less disk |
| fold_mixed | 1445.930 | 1296.189 | 0.896 | 0.896 | 1.000 | – | faster |
| twin_distinct_int4_k37 | 2153.667 | 2153.711 | 1.000 | 1.000 | 1.000 | 1.000 |  |

Runs: base pg-aggfold-4QXgZI; candidate pg-aggfold-lMmIjx.

### ab-NKyjhS

| case | base, M instructions | candidate, M instructions | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| guard_float | 3777.940 | 3778.863 | 1.000 | 1.000 | 1.000 |  |
| guard_lone | 267.937 | 268.040 | 1.000 | 1.000 | 1.000 |  |
| guard_parallel | 1749.218 | 261.437 | – | – | 1.000 | **plan differs** |
| guard_tie | 338.083 | 309.722 | 0.916 | 0.916 | 1.000 | faster |

Runs: base pg-aggfold-V7YIQA; candidate pg-aggfold-VUOcsO.

### ab-zABvvu

| case | base, M instructions | candidate, M instructions | min | median | control | disk | |
|---|---:|---:|---:|---:|---:|---:|---|
| fold_ten_int4_spill | 6763.228 | 4676.797 | 0.692 | 0.692 | 1.000 | 0.181 | faster, less disk |
| twin_ten_int4_spill | 3416.576 | 3450.024 | 1.010 | 1.010 | 1.001 | 1.000 |  |

Runs: base pg-aggfold-9L5lU1; candidate pg-aggfold-r54Cxd.

