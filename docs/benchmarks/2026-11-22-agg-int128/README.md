# agg-int128: the measurements

The runs of the measurement plan of the OpenSpec change `agg-int128`:
`sum` and `avg` of `bigint` values of 19 digits in the kernels' 128-bit
sum, instead of the core's `numeric_add`, ClickBench's query 4 among
them. The change touches code inside the kernels and leaves the plans
as they are, so counts decide; one run of times answers the claim
against the core.

- Machine: a MacBook Pro, Apple M5 Pro, on mains power, macOS.
- PostgreSQL 20devel built for release, without assertions; the base
  `624937e9`, the commit that adds the cases, the code without the new
  terms; the candidate `493ad715`, a commit of the branch before its
  history was put in order, as the reports name it.
- The family `anyagg` of `bench/pg` with the table `bench_digits`:
  1 000 000 rows, a `bigint` column of 19 digits of both signs and its
  twin of 18 digits, summed without groups (`g_sum_digits19`,
  `g_avg_digits19`, the shape of query 4), by keys in runs of 64 rows
  (`g_runs_digits19`) and by keys that follow one another
  (`g_spread_digits19`), each beside its twin of 18 digits.

## Hypotheses

Written before any run:

1. On the base each case of 19 digits runs several times the
   instructions of its twin of 18; on the candidate it runs within 5 %
   of its twin.
2. Every other case of `anyagg`, and the twins, run within 1 % of the
   base's instructions, or fewer.
3. With Tessera on, the cases of 19 digits take less time than with it
   off on the candidate; on the base they may take more.

## Results

- The machine code ([kernels](disasm/kernels.txt),
  [programs](disasm/programs.txt)): every program of `tessera-bench`
  the same. The loops of `tess_decimal_sum` are no longer than the
  base's, those of `tess_table_accumulate_extremes` the base's. The
  body of `tess_table_accumulate_sums` moved into its closure, called
  once a call, its innermost loops 5, 6, 7 and 8 instructions against
  5, 6, 7 and 7: its counts judge it.
- The counts ([1-counts](ab/1-counts.md)), the control at 1.000 in
  every case. Hypothesis 1 holds: the cases of 19 digits fall from 1095
  to 117 million instructions without groups (0.107), from 1369 to 353
  by runs of keys (0.258) and from 1373 to 416 by keys that follow one
  another (0.303), each within 0.5 % of its twin. Hypothesis 2 holds
  but for three groupings whose rows go to their records one by one,
  `g_group_sum`, `g_group_sum_numeric` and `g_spread_digits18`, 1.4 %
  more; `g_group_int2` 1.0 %, `g_sum_numeric` 0.7 %; eleven cases fall,
  by up to 4 %.
- The three past the rule of 1 % pay for the count kept within an
  int64: 0.6 to 0.8 % before it ([2](ab/2-before-the-count.md)), 1.0 %
  without its comparison in the fast path of `SumState::add_to`
  ([3](ab/3-count-without-fast-compare.md)); comparing the count after
  its increment by its sign did not change the code
  ([4](ab/4-count-by-sign.md)). The maintainer chose to keep the check
  on every row and to accept the 1.4 %; their times do not move, 0.997
  of the base over three rounds.
- The way there. The first version failed hypothesis 2 on five cases,
  by 1.2 to 1.9 % ([5](ab/5-first-version.md)); the new terms alone
  stayed within 0.1 % ([6](ab/6-terms-alone.md)). The decimal's fast
  path of `SumState::add_to` restored, the integer's apart, brought the
  groupings to 0.6 to 0.8 % ([7](ab/7-decimal-fast-path.md)); the plain
  loop's arm of an integer kept out of line brought the sum of
  `numeric` from 1.2 % to 0.1 % ([8](ab/8-plain-arm-out-of-line.md)).
- The times ([9-times](ab/9-times.md), three rounds of 11). Hypothesis
  3 holds: with Tessera on over off, by the means of the medians, `sum`
  of 19 digits 1.89 on the base and 0.23 on the candidate, `avg` 1.88
  and 0.23, by runs of keys 1.14 and 0.35, by keys that follow one
  another 0.93 and 0.28; over 1 M rows `sum` and `avg` of 19 digits
  take 35 ms on the base and 4.3 ms on the candidate. One round of the
  first version read a twin 1.033 of the base
  ([10](ab/10-first-version-times.md)), three rounds 1.007
  ([11](ab/11-first-version-times-3.md)), as its counts.

## Files

- `ab/`: the reports of `cargo ab measure`, numbered as cited above;
  the raw runs stay in `target/bench-runs/` of the machine.
- `disasm/`: the reports of `tessera-bench --disasm` for the kernels'
  module and for every program.
