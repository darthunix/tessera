# aggfold by instructions, one round: base a71b632a, candidate 68e233db

### ab-M54sNQ

| case | base, M instructions | candidate, M instructions | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| fold_ten_int2 | 291.946 | 291.448 | 0.998 | 0.998 | 1.000 |  |
| fold_two_int4 | 261.394 | 261.211 | 0.999 | 0.999 | 1.000 |  |
| twin_ten_int2 | 291.924 | 291.604 | 0.999 | 0.999 | 1.000 |  |
| twin_two_int4 | 241.036 | 240.848 | 0.999 | 0.999 | 1.000 |  |

Runs: base pg-aggfold-Xgqzdx; candidate pg-aggfold-5k87NX.

### ab-92sik5

| case | base, M instructions | candidate, M instructions | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| fold_ten_int4 | 261.447 | 261.250 | 0.999 | 0.999 | 1.000 |  |
| fold_ten_int8 | 269.162 | 269.169 | 1.000 | 1.000 | 1.000 |  |
| twin_ten_int4 | 303.072 | 302.683 | 0.999 | 0.999 | 1.000 |  |
| twin_ten_int8 | 314.637 | 314.455 | 0.999 | 0.999 | 0.998 |  |

Runs: base pg-aggfold-OYtABv; candidate pg-aggfold-4s2lU8.

### ab-C1UX4e

| case | base, M instructions | candidate, M instructions | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| fold_kinds_int4 | 323.451 | 323.073 | 0.999 | 0.999 | 1.000 |  |
| fold_ten_numeric | 638.643 | 638.917 | 1.000 | 1.000 | 1.000 |  |
| twin_kinds_int4 | 303.052 | 302.687 | 0.999 | 0.999 | 1.000 |  |
| twin_ten_numeric | 638.622 | 638.620 | 1.000 | 1.000 | 1.000 |  |

Runs: base pg-aggfold-k6YkUy; candidate pg-aggfold-3I4DQH.

### ab-DdVcgB

| case | base, M instructions | candidate, M instructions | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| fold_ten_abs | 418.932 | 418.575 | 0.999 | 0.999 | 1.000 |  |
| fold_ten_int4_k37 | 1004.525 | 1004.296 | 1.000 | 1.000 | 1.000 |  |
| twin_ten_abs | 398.769 | 398.038 | 0.998 | 0.998 | 1.000 |  |
| twin_ten_int4_k37 | 1210.804 | 1210.475 | 1.000 | 1.000 | 1.000 |  |

Runs: base pg-aggfold-BzO44Z; candidate pg-aggfold-Qamlff.

### ab-13vhiX

| case | base, M instructions | candidate, M instructions | min | median | control | disk | |
|---|---:|---:|---:|---:|---:|---:|---|
| fold_ten_int4_m1m | 4676.712 | 4708.421 | 1.007 | 1.007 | 0.998 | 1.000 |  |
| twin_ten_int4_m1m | 3449.557 | 3508.005 | 1.017 | 1.017 | 0.999 | 1.000 | **slower** |

Runs: base pg-aggfold-4EsIm4; candidate pg-aggfold-YalRIW.

### ab-zn6hFO

| case | base, M instructions | candidate, M instructions | min | median | control | disk | |
|---|---:|---:|---:|---:|---:|---:|---|
| fold_ten_numeric_m1m | 13911.758 | 13912.600 | 1.000 | 1.000 | 1.000 | 1.000 |  |
| twin_ten_numeric_m1m | 16513.967 | 16524.893 | 1.001 | 1.001 | 1.000 | 1.000 |  |

Runs: base pg-aggfold-TrvAlo; candidate pg-aggfold-2yoGsy.

### ab-5wbQQx

| case | base, M instructions | candidate, M instructions | min | median | control | disk | |
|---|---:|---:|---:|---:|---:|---:|---|
| fold_distinct_int4_k37 | 2173.973 | 2174.118 | 1.000 | 1.000 | 1.001 | 1.000 |  |
| fold_mixed | 1296.218 | 1296.004 | 1.000 | 1.000 | 1.000 | – |  |
| twin_distinct_int4_k37 | 2154.010 | 2153.989 | 1.000 | 1.000 | 1.000 | 1.000 |  |

Runs: base pg-aggfold-i6S9w4; candidate pg-aggfold-LqQyhy.

### ab-ghrwh5

| case | base, M instructions | candidate, M instructions | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| guard_float | 3778.863 | 3778.820 | 1.000 | 1.000 | 1.000 |  |
| guard_lone | 268.042 | 267.811 | 0.999 | 0.999 | 1.000 |  |
| guard_parallel | 261.438 | 261.250 | 0.999 | 0.999 | 1.000 |  |
| guard_tie | 309.742 | 309.535 | 0.999 | 0.999 | 1.000 |  |

Runs: base pg-aggfold-zFy6cI; candidate pg-aggfold-l3CoW6.

### ab-CI2008

| case | base, M instructions | candidate, M instructions | min | median | control | disk | |
|---|---:|---:|---:|---:|---:|---:|---|
| fold_ten_int4_spill | 4676.351 | 4708.825 | 1.007 | 1.007 | 1.000 | 1.000 |  |
| twin_ten_int4_spill | 3448.840 | 3508.000 | 1.017 | 1.017 | 0.999 | 1.000 | **slower** |

Runs: base pg-aggfold-eoedGy; candidate pg-aggfold-bKOAit.
