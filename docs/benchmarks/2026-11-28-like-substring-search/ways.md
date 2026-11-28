# The ways of 3.1 against the base 7d64c988, family like

Each way is a branch that is never merged, counted by `cargo ab measure
--base 7d64c988 --candidate <branch> --family like --counters
--cycles --rounds 2 --deadline 300`, then `--counters` alone for the
instructions; PostgreSQL 19 with pg_pmu, the M5 Pro on mains, idle at
the maintainer's word. Ratios are the candidate's over the base's;
below 1 is less.

## way 1, a memmem searcher a piece in Like (24941ba9)

### cycles, ab-OkkoAB

| case | base, M cycles | candidate, M cycles | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| guard_exact | 85.434 | 90.504 | 1.047 | 1.059 | 1.012 | **slower** |
| guard_prefix | 93.465 | 93.051 | 0.988 | 0.996 | 0.999 |  |
| guard_suffix | 106.774 | 110.373 | 1.030 | 1.034 | 0.998 | **slower** |
| guard_underscore | 595.246 | 588.904 | 0.990 | 0.989 | 0.996 |  |
| like_near_start | 118.989 | 117.463 | 1.001 | 0.987 | 1.012 |  |
| like_one_byte | 120.760 | 112.439 | 0.928 | 0.931 | 1.014 | faster |
| like_short | 194.923 | 146.827 | 0.746 | 0.753 | 0.998 | faster |
| like_two_pieces | 406.553 | 169.235 | 0.414 | 0.416 | 1.059 | faster |
| like_url_google | 211.003 | 132.735 | 0.630 | 0.629 | 0.989 | faster |

Runs: base pg-like-Tql7K3, pg-like-u9l84r; candidate pg-like-RRoVOR, pg-like-ql3u2e.

### instructions, ab-mfEpAP

| case | base, M instructions | candidate, M instructions | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| guard_exact | 528.951 | 545.981 | 1.032 | 1.032 | 1.000 | **slower** |
| guard_prefix | 612.408 | 622.972 | 1.017 | 1.017 | 1.000 | **slower** |
| guard_suffix | 662.540 | 674.843 | 1.019 | 1.019 | 1.000 | **slower** |
| guard_underscore | 2573.543 | 2575.301 | 1.001 | 1.001 | 1.000 |  |
| like_near_start | 857.283 | 868.172 | 1.013 | 1.013 | 1.000 | **slower** |
| like_one_byte | 802.687 | 754.355 | 0.940 | 0.940 | 1.000 | faster |
| like_short | 923.878 | 1015.380 | 1.099 | 1.099 | 1.000 | **slower** |
| like_two_pieces | 1211.124 | 890.355 | 0.735 | 0.735 | 1.000 | faster |
| like_url_google | 873.120 | 879.827 | 1.008 | 1.008 | 1.000 |  |

Runs: base pg-like-8B4e2m; candidate pg-like-6WcdwO.

## way 2, memmem::find a row (3a58d2d8)

### cycles, ab-Zk7uMu

| case | base, M cycles | candidate, M cycles | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| guard_exact | 86.226 | 87.565 | 1.020 | 1.016 | 1.007 |  |
| guard_prefix | 91.488 | 95.344 | 1.030 | 1.042 | 0.940 | **slower** |
| guard_suffix | 106.838 | 108.342 | 0.989 | 1.014 | 0.990 |  |
| guard_underscore | 593.855 | 589.437 | 0.989 | 0.993 | 0.987 |  |
| like_near_start | 119.213 | 154.495 | 1.274 | 1.296 | 0.991 | **slower** |
| like_one_byte | 121.399 | 202.750 | 1.681 | 1.670 | 1.009 | **slower** |
| like_short | 195.423 | 172.567 | 0.882 | 0.883 | 1.012 | faster |
| like_two_pieces | 408.921 | 312.074 | 0.764 | 0.763 | 0.988 | faster |
| like_url_google | 208.262 | 266.254 | 1.278 | 1.278 | 0.985 | **slower** |

Runs: base pg-like-ZdEeaj, pg-like-o2CHtm; candidate pg-like-gYi3db, pg-like-2Jtyy7.

### instructions, ab-ec7WjG

| case | base, M instructions | candidate, M instructions | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| guard_exact | 528.952 | 529.689 | 1.001 | 1.001 | 1.000 |  |
| guard_prefix | 612.396 | 616.560 | 1.007 | 1.007 | 1.000 |  |
| guard_suffix | 662.542 | 665.860 | 1.005 | 1.005 | 1.000 |  |
| guard_underscore | 2573.553 | 2573.561 | 1.000 | 1.000 | 1.000 |  |
| like_near_start | 857.282 | 1101.483 | 1.285 | 1.285 | 1.000 | **slower** |
| like_one_byte | 802.683 | 908.166 | 1.131 | 1.131 | 1.000 | **slower** |
| like_short | 923.913 | 973.267 | 1.053 | 1.053 | 1.000 | **slower** |
| like_two_pieces | 1211.105 | 1498.742 | 1.237 | 1.237 | 1.000 | **slower** |
| like_url_google | 873.123 | 1551.908 | 1.777 | 1.777 | 1.000 | **slower** |

Runs: base pg-like-fUHVmR; candidate pg-like-cQYxfM.

## way 3, memchr of the rarest byte (735b51ec)

### cycles, ab-rfet2D

| case | base, M cycles | candidate, M cycles | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| guard_exact | 88.159 | 91.061 | 1.013 | 1.033 | 1.081 |  |
| guard_prefix | 93.277 | 103.215 | 0.987 | 1.107 | 1.062 |  |
| guard_suffix | 107.200 | 114.625 | 0.989 | 1.069 | 1.052 |  |
| guard_underscore | 591.718 | 609.093 | 1.008 | 1.029 | 1.068 |  |
| like_near_start | 117.589 | 132.054 | 1.048 | 1.123 | 1.145 |  |
| like_one_byte | 120.778 | 135.636 | 1.040 | 1.123 | 1.126 |  |
| like_short | 195.314 | 204.476 | 1.023 | 1.047 | 1.068 |  |
| like_two_pieces | 408.863 | 301.458 | 0.717 | 0.737 | 1.061 | faster |
| like_url_google | 210.658 | 220.699 | 1.021 | 1.048 | 1.030 | **slower** |

Runs: base pg-like-8ilopB, pg-like-uqTwPN; candidate pg-like-aun8Nc, pg-like-EWIW2i.

### instructions, ab-uBzWka

| case | base, M instructions | candidate, M instructions | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| guard_exact | 528.949 | 535.559 | 1.013 | 1.012 | 1.000 | **slower** |
| guard_prefix | 612.387 | 620.144 | 1.013 | 1.013 | 1.000 | **slower** |
| guard_suffix | 662.551 | 665.307 | 1.004 | 1.004 | 1.000 |  |
| guard_underscore | 2573.567 | 2574.448 | 1.000 | 1.000 | 1.000 |  |
| like_near_start | 857.279 | 872.690 | 1.018 | 1.018 | 1.000 | **slower** |
| like_one_byte | 802.683 | 816.769 | 1.018 | 1.018 | 1.000 | **slower** |
| like_short | 923.892 | 929.876 | 1.006 | 1.006 | 1.000 |  |
| like_two_pieces | 1211.133 | 950.064 | 0.784 | 0.784 | 1.000 | faster |
| like_url_google | 873.129 | 881.521 | 1.010 | 1.010 | 1.000 |  |

Runs: base pg-like-pPQDdV; candidate pg-like-fZXzZ1.

## way 1b, the searchers of way 1 boxed, made only for a pattern that searches (f339520b)

### cycles, ab-c4sX4x

| case | base, M cycles | candidate, M cycles | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| guard_exact | 85.562 | 86.021 | 1.017 | 1.005 | 1.005 |  |
| guard_prefix | 92.076 | 91.015 | 0.976 | 0.988 | 1.005 | faster |
| guard_suffix | 106.897 | 107.960 | 0.991 | 1.010 | 0.995 |  |
| guard_underscore | 591.481 | 592.585 | 1.003 | 1.002 | 0.998 |  |
| like_near_start | 119.208 | 118.368 | 1.002 | 0.993 | 0.998 |  |
| like_one_byte | 121.669 | 113.094 | 0.927 | 0.930 | 1.000 | faster |
| like_short | 193.925 | 146.041 | 0.754 | 0.753 | 0.999 | faster |
| like_two_pieces | 409.856 | 171.623 | 0.422 | 0.419 | 1.003 | faster |
| like_url_google | 205.484 | 134.909 | 0.656 | 0.657 | 1.003 | faster |

Runs: base pg-like-rUe4HW, pg-like-vAvA52; candidate pg-like-s5XX6T, pg-like-SLGuM5.

### instructions, ab-AVOdAA

| case | base, M instructions | candidate, M instructions | min | median | control | |
|---|---:|---:|---:|---:|---:|---|
| guard_exact | 528.952 | 529.684 | 1.001 | 1.001 | 1.000 |  |
| guard_prefix | 612.400 | 614.150 | 1.003 | 1.003 | 1.000 |  |
| guard_suffix | 662.549 | 662.708 | 1.000 | 1.000 | 1.000 |  |
| guard_underscore | 2573.571 | 2574.073 | 1.000 | 1.000 | 1.000 |  |
| like_near_start | 857.291 | 881.373 | 1.028 | 1.028 | 1.000 | **slower** |
| like_one_byte | 802.683 | 767.585 | 0.956 | 0.956 | 1.000 | faster |
| like_short | 923.895 | 1032.390 | 1.117 | 1.117 | 1.000 | **slower** |
| like_two_pieces | 1211.140 | 903.217 | 0.746 | 0.746 | 1.000 | faster |
| like_url_google | 873.121 | 896.984 | 1.027 | 1.027 | 1.000 | **slower** |

Runs: base pg-like-iuxg5B; candidate pg-like-pbWinp.

## The machine code

`cargo run --release -p tessera-bench -- --base 7d64c988 --candidate
<branch> --disasm --module kernels --function tess_text_like`: the
kernels' module as make builds it, the Rust part of one codegen unit.

- `like-way-finder` (disasm-tnEvB7): kernels: 3259 same, 8 changed (0 in
  immediates only, 0 in registers only), 0 only before, 17 only after;
  changed beside the formatting of
  errors: `<tessera_kernels::text::Like>::parse`, `_tess_text_like`,
  `tessera_kernels::accumulate::combine`.
- `like-way-find` (disasm-j8yqvC): kernels: 3263 same, 4 changed (0 in
  immediates only, 0 in registers only), 0 only before, 17 only after;
  changed beside the formatting of
  errors: `_tess_text_like`.
- `like-way-rarest` (disasm-Mo4Us9): kernels: 3263 same, 4 changed (0 in
  immediates only, 0 in registers only), 0 only before, 0 only after;
  changed beside the formatting of
  errors: `<tessera_kernels::text::Like>::parse`, `_tess_text_like`.
- `like-way-finder-box` (disasm-Q2CIsH): kernels: 3259 same, 8 changed
  (0 in immediates only, 0 in registers only), 0 only before, 17 only
  after; changed beside the formatting of
  errors: `<tessera_kernels::text::Like>::parse`, `_tess_text_like`,
  `tessera_kernels::accumulate::combine`.

In ways 1 and 1b `accumulate::combine`, the merge of records read back
after a spill, which the change does not touch, keeps its 523
instructions; its cold blocks of errors stand in another order, and
its entry moves from the offset 48 to 8 of a 64-byte line.

## The way kept

Way 1 found the pieces in the fewest cycles, but its four searchers in
`Like`, made, moved and dropped by every call, took the guards that
search nothing 3 to 6 per cent more cycles. The hypothesis written
before way 1b: boxed and made only for a pattern with an inner piece,
the searchers cost the guards nothing, and the searching cases keep
their gains. It held: the guards at 0.99 to 1.01 of the base's cycles
and 1.000 to 1.003 of its instructions; TPC-H's predicate at 0.42 of the
cycles, `'%google%'` 0.66, the short strings 0.75. The short strings
take 12 per cent more instructions, the making of a searcher once a
call, some 3 500 instructions a batch of 64 rows, for 25 per cent fewer
cycles. Way 1b is kept.
