# The family like at the end: base cfcbfecf (2.1), end 18f5e5cb (3.2)

### cycles, ab-AloeJ5

| case | base, M cycles | candidate, M cycles | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| guard_exact | 87.396 | 86.226 | 0.977 | 0.987 | 1.003 | faster |
| guard_prefix | 92.980 | 92.488 | 1.019 | 0.995 | 0.954 | **slower** |
| guard_suffix | 109.331 | 108.448 | 0.989 | 0.992 | 0.997 |  |
| guard_underscore | 594.387 | 589.949 | 0.985 | 0.993 | 0.990 |  |
| like_near_start | 128.973 | 120.595 | 0.996 | 0.935 | 0.960 |  |
| like_one_byte | 122.314 | 113.545 | 0.924 | 0.928 | 1.005 | faster |
| like_short | 203.734 | 148.167 | 0.748 | 0.727 | 1.004 | faster |
| like_two_pieces | 405.794 | 176.807 | 0.421 | 0.436 | 1.000 | faster |
| like_url_google | 207.622 | 137.479 | 0.654 | 0.662 | 1.008 | faster |

Runs: base pg-like-LTxjxZ, pg-like-qIdLPM; candidate pg-like-WsD1yz, pg-like-rIZJSw.

### instructions, ab-sIDNhJ

| case | base, M instructions | candidate, M instructions | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| guard_exact | 528.954 | 529.673 | 1.001 | 1.001 | 1.000 |  |
| guard_prefix | 612.403 | 614.142 | 1.003 | 1.003 | 1.000 |  |
| guard_suffix | 662.535 | 662.705 | 1.000 | 1.000 | 1.000 |  |
| guard_underscore | 2573.577 | 2574.061 | 1.000 | 1.000 | 1.000 |  |
| like_near_start | 857.306 | 881.346 | 1.028 | 1.028 | 1.000 | **slower** |
| like_one_byte | 802.681 | 767.587 | 0.956 | 0.956 | 1.000 | faster |
| like_short | 923.878 | 1032.402 | 1.117 | 1.117 | 1.000 | **slower** |
| like_two_pieces | 1211.135 | 903.204 | 0.746 | 0.746 | 1.000 | faster |
| like_url_google | 873.109 | 896.972 | 1.027 | 1.027 | 1.000 | **slower** |

Runs: base pg-like-fJtXcJ; candidate pg-like-4w2mdN.
