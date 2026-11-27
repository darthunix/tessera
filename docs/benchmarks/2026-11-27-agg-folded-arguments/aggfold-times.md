# aggfold by time: base 457b80d9, candidate da90e0c6, 5 repetitions

### ab-yNQIXB

| case | base, ms | candidate, ms | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| fold_ten_int4_spill | 279.140 | 168.005 | 0.602 | 0.602 | 0.964 | faster |

Runs: base pg-aggfold-rIoNjb, pg-aggfold-K3U71G; candidate pg-aggfold-EPYrjX, pg-aggfold-jG6I7p.

### ab-vZBGhY

| case | base, ms | candidate, ms | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| twin_ten_int4_spill | 142.332 | 142.899 | 0.998 | 1.004 | 1.004 |  |

Runs: base pg-aggfold-wcVORC, pg-aggfold-CK1igY; candidate pg-aggfold-59kjRv, pg-aggfold-FUr1dC.

### ab-bZsAmq

| case | base, ms | candidate, ms | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| guard_parallel | 28.582 | 9.733 | 0.341 | 0.341 | 1.098 | faster |

Runs: base pg-aggfold-ibGTa7, pg-aggfold-G4iAm6; candidate pg-aggfold-bbToCT, pg-aggfold-JXDFNk.

### ab-HqTdBS

| case | base, ms | candidate, ms | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| fold_ten_numeric_m1m | 1802.529 | 440.666 | 0.244 | 0.244 | 1.019 | faster |

Runs: base pg-aggfold-dsjEYj, pg-aggfold-KNlJkC; candidate pg-aggfold-8FHrN5, pg-aggfold-6W6In3.

