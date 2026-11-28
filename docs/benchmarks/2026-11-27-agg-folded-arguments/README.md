# Aggregates folded from shared states, `agg-folded-arguments`, 2026-11-27

The count of the first stage of the OpenSpec change
`agg-folded-arguments`. `TessAgg` reads an aggregate's argument as a
form `a·e + b` of an expression `e`, keeps the states of `e` its
aggregates share, the sum and the count, the extremes, one set of
`DISTINCT` pairs, `count(*)` for constants, and derives each aggregate
from them; a form that can pass its type keeps a check of errors, the
least and the greatest `e` of the input. Cycles decide, by the
maintainer's rule; instructions stand beside them, and times stand for
the claim of speed and for the cases whose plans differ.

- **Tessera**, release builds: the base `457b80d9`, the commit of task
  2.1, which adds the family `aggfold` and changes nothing of the node;
  the end `da90e0c6`, after the fixes of the code review and the check
  of errors in one pass. `cargo ab` builds both sides in its own
  directories; ClickBench's base is a worktree at `457b80d9`, its end
  the branch's worktree, built clean for the server below.
- **PostgreSQL** 19rc2, release, without LLVM, with pg_pmu and
  pg_duckdb 1.2.0: the binaries of the counts of 2026-11-24 to
  2026-11-27, by their hashes in the sources below.
- **`aggfold`**: its own table of 2 million rows, `cargo ab measure
  --base 457b80d9 --family aggfold --cases <group> --counters --cycles
  --rounds 2 --deadline 300`, then `--counters` alone for the
  instructions, nine groups of cases of five minutes at most; three
  readings a case and mode, base and candidate alternating. The times:
  `cargo ab measure` without counters, five repetitions, of the cases
  that spill and of those whose plans differ.
- **ClickBench**: `hits.parquet`, 100 M rows, 14.8 GB, in the page
  cache. `cargo clickbench run --mode <on|off> --counters --tries 5
  --workers 0 --queries <n> --files hits.parquet`, one query a run, five
  readings after one that warms up, their medians; the base and the end
  in turns, query by query, `base`, `end`, then `base-b`, `end-b` in
  [clickbench-runs.txt](clickbench-runs.txt); pg_duckdb's counts with
  one thread of DuckDB; the times of Q30, five tries, after the counts.
- **Machine**: Apple M5 Pro, 18 cores, 64 GB, idle at the maintainer's
  word, on mains. Before each run a script waited for a minute in which
  no process but the agent's took more than 60 per cent of a core: a
  chat application held some 46 per cent of one all evening.

## What went wrong on the way

- The Mac went into idle sleep at 21:12 during the first chain of the
  final series; its first run stands, the chain was stopped and run
  again under `caffeinate -i`, and the runs of both are kept.
- The first ClickBench run of the end failed: the branch's worktree
  held objects built for PostgreSQL master by the test suites, and the
  tool installed them into the server of 19, which refused the library
  ([clickbench-runs-failed.txt](clickbench-runs-failed.txt)). The
  worktree was cleaned and the whole ClickBench step run again.

## Hypotheses

Written in the design before the runs:

1. Q30 leaves the core's aggregate for `TessAgg` with two states: at
   least 90 per cent fewer cycles and less time.
2. The folded cases: within 10 per cent of their twins' cycles at the
   end, and fewer than at the base: two forms 0 to 30 per cent fewer,
   ten forms 50 to 85.
3. The cases with a million groups: fewer cycles than at the base, and
   within 20 per cent of their twins'.
4. The spilled cases: no more time and no more bytes written than at
   the base.
5. The guards: within 2 per cent; Q3, Q4 and the control within 2.

A stage is kept when hypotheses 2 to 4 hold and no case takes more than
2 per cent more cycles or time than its base.

## The first series, and the check of errors

The first series of cycles, at `d55788d2`
([aggfold-first-series.md](aggfold-first-series.md)), found the check of
errors dear: the cases that keep it, `sum(i), sum(i + 1)`, ten
aggregates of every kind and ten sums of `abs(i) + k`, took 18 to 19
million cycles more than their twins over 2 million rows, about 9 a
row, and `fold_two_int4` 6 per cent more than the base. The check read
each row through a call and two branches. Two ways were counted on
those cases:

- the kernels' batch functions of `min` and `max`, two calls a batch:
  some 6 cycles a row, still 17 to 30 per cent over the twins
  (`ab-9tNs31`);
- one pass over the column for both extremes, a word of 64 rows read
  whole without a branch a row: some 1.6 cycles a row, 4.5 to 7.9 per
  cent over the twins (`ab-uqma2A`), kept as `da90e0c6`.

## Result: `aggfold`

Millions of cycles at the base and at the end, by the medians of two
rounds a side ([aggfold-cycles.md](aggfold-cycles.md)); the
instructions ([aggfold-instructions.md](aggfold-instructions.md)) move
the same way within a few per cent, the guards' not at all. The cases:

- `two_int4`: `sum(i)`, `sum(i + 1)`;
- `ten_int2`, `ten_int4`, `ten_int8`, `ten_numeric`: ten sums of
  `s + k` of a smallint, `i * 2 + k` of an integer, `b + k` of a bigint,
  `n + k` of a numeric(12,2);
- `kinds_int4`: ten aggregates of every kind of `i`;
- `ten_abs`: ten sums of `abs(i) + k`;
- `ten_int4_k37`, `ten_int4_m1m`, `ten_numeric_m1m`: ten sums in 37
  groups and in a million;
- `distinct_int4_k37`: three `DISTINCT` forms in 37 groups;
- `mixed`: three forms beside three aggregates that do not fold;
- `ten_int4_spill`: ten sums, a million groups past `work_mem`.

Each `fold_` case beside its `twin_`, which keeps the same states and
derives nothing; lower is better:

| case | base | end | end / base | end / twin |
|---|--:|--:|--:|--:|
| `two_int4` | 55.9 | 45.1 | 0.81 | 1.08 |
| `ten_int2` | 212.1 | 52.6 | 0.25 | 1.00 |
| `ten_int4` | 315.8 | 45.8 | 0.15 | 0.84 |
| `ten_int8` | 237.6 | 42.3 | 0.18 | 0.79 |
| `ten_numeric` | 539.3 | 85.0 | 0.16 | 1.02 |
| `kinds_int4` | 223.4 | 58.0 | 0.26 | 1.06 |
| `ten_abs` | 525.3 | 76.5 | 0.15 | 1.05 |
| `ten_int4_k37` | 500.9 | 152.1 | 0.30 | 0.84 |
| `ten_int4_m1m` | 1 271.8 | 768.2 | 0.60 | 1.18 |
| `ten_numeric_m1m` | 8 214.7 | 1 998.7 | plan differs | 0.76 |
| `distinct_int4_k37` | 1 122.5 | 459.1 | 0.41 | – |
| `mixed` | 216.3 | 186.7 | 0.86 | – |
| `ten_int4_spill` | 1 270.0 | 760.4 | 0.60 | 1.18 |

The twins moved too, up to 31 per cent fewer cycles, as the equal
arguments of their states share one computed column now (task 2.2).
The twin of `fold_ten_int2`, `sum(s::integer), count(s)`, folds two
aggregates itself: its states are the same.

The guards, which fold nothing: `lone`, `sum(i + 1)` alone; `tie`,
`sum(i)`, `avg(i)`, `count(i)`; `float`, ten forms of a float;
`parallel`, ten forms in a parallel plan:

| case | base | end | end / base | control |
|---|--:|--:|--:|--:|
| `lone` | 47.6 | 48.0 | 1.008 / 1.009 | 1.018 |
| `tie` | 57.0 | 52.8 | 0.912 / 0.927 | 1.006 |
| `float` | 457.9 | 466.1 | 1.016 / 1.018 | 0.996 |
| `float`, third pair | 459.8 | 468.3 | 1.003 / 1.019 | 1.003 |
| `parallel` | 349.7 | 44.5 | plan differs | 0.974 |

`end / base` is by the minimum and by the median. `guard_float` passed
1 per cent in the series and was counted a third pair: its minimum did
not move, its median 1.6 per cent beyond the control, its instructions
not at all (1.000). `guard_parallel` no longer runs in parallel: the
planner takes the serial path that folds, cheaper by its costs.

The times ([aggfold-times.md](aggfold-times.md)), milliseconds, medians:

| case | base | end | end / base | disk |
|---|--:|--:|--:|--:|
| `fold_ten_int4_spill` | 279.1 | 168.0 | 0.60 | 0.18 |
| `twin_ten_int4_spill` | 142.3 | 142.9 | 1.00 | 1.00 |
| `guard_parallel` | 28.6 | 9.7 | 0.34 | – |
| `fold_ten_numeric_m1m` | 1 802.5 | 440.7 | 0.24 | 0.20 |

At the base `fold_ten_numeric_m1m` groups in the core's hashed
aggregation, which the planner preferred to `TessAgg` with ten sum
states; at the end in `TessAgg` with two, its rows past `work_mem` on
disk, 23 MB written where the core wrote 114.

### `fold_ten_numeric_m1m` against its twin

The first twin of the case kept the same states, `avg(n)` and
`count(n)`, but gave two values a group where the case gives ten
numerics, which its reading adds: the case took 1.89 of its cycles. A
probe of four statements at the end, the reading counting the ten sums
in place of adding them, millions of cycles with Tessera on
([probe-numeric-m1m.txt](probe-numeric-m1m.txt)):

| statement | folded | twin | difference |
|---|--:|--:|--:|
| the outer query adds the sums | 1 993 | 1 072 | 921 |
| the outer query counts them | 1 620 | 794 | 826 |

The additions are a tenth of the difference; the rest is the ten
numerics a group the derivation makes and the output copies, some 80
cycles each, which the base makes too, one a sum state. At the
maintainer's word the twin was made to read as the case does
(`fb7c6178`): the same states, `sum(n)` and `count(n)`, and the same
ten values computed from them above the aggregates, `sum(n) + k *
count(n)`, which the reading adds. Counted again in two rounds
(`ab-cntjxv`), the case takes 2 006.8 million cycles and the twin
2 638.6: 0.76 of the twin's, the fold deriving the values cheaper than
the core's executor computes them. The ratio in the table above is
this one.

## Result: ClickBench

Q30 sums `ResolutionWidth` and `ResolutionWidth + k` for `k` from 1 to
89; Q3 is `SUM`, `COUNT(*)` and `AVG` of two columns, Q4 `AVG(UserID)`,
guards that fold nothing; Q1 with Tessera off is the control. Billions
of cycles at the base and at the end in each pair, the change, and
pg_duckdb's at the end; lower is better:

| query | base | end | change, by pair | pg_duckdb |
|---|--:|--:|--:|--:|
| Q30 | 130.29 / 131.41 | 2.231 / 2.227 | -98.3, -98.3 % | 0.826 |
| Q3 | 3.177 / 3.188 | 3.198 / 3.219 | +0.7, +1.0 % | 0.936 |
| Q4 | 1.793 / 1.795 | 1.803 / 1.799 | +0.6, +0.2 % | 2.930 |
| Q1, off | 23.93 / 23.89 | 23.87 / 23.58 | -0.2, -1.3 % | – |

Instructions, billions: Q30 699.2 and 12.24; Q3 20.00 and 20.01, 20.10;
Q4 10.93 and 10.93; Q1 off 123.71 and 123.71. The time of Q30, five
tries: 29.0 s at the base, 0.56 s at the end.

## Verdict

1. Holds: Q30 takes 98.3 per cent fewer cycles and 52 times less time;
   it is 2.7 times pg_duckdb's cycles, where it was 158 times.
2. Holds: every folded case is within 8 per cent of its twin, and takes
   19 per cent fewer cycles than the base with two forms and 70 to 86
   with ten; `ten_int4` and `ten_abs` pass the range's 85 per cent by
   half a point.
3. Holds: the integers take 40 per cent fewer cycles than the base and
   18 per cent more than the twin; the numerics a quarter of the
   base's time and 0.76 of the cycles of the twin that computes the
   same values, after the first twin, which computed two, was fixed.
4. Holds: the spilled case takes 0.60 of the base's time and writes
   0.18 of its bytes.
5. Holds: the guards within 2 per cent, `guard_float` with its third
   pair, Q3 and Q4 within their control.

No case takes more than 2 per cent more cycles or time than its base.
