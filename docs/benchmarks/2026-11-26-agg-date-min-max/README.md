# Extremes and keys of the word types, `agg-date-min-max`, 2026-11-26

The count of the OpenSpec change `agg-date-min-max`. TessAgg computes
`min` and `max` of `smallint`, `date`, `timestamp`, `timestamptz`,
`time` and `money`, and `bool_and`, `bool_or` and `every`, by the
kernels of the integers instead of a call of the core's function a
row. It holds `time` and `money` as integers of eight bytes in its
comparisons, lists of `IN` and keys. And it ignores `DISTINCT` in those
extremes. Cycles decide, by the maintainer's rule; instructions stand
beside them.

- **Tessera**, release builds: the base `f708ef0d`, the commit of task
  2.1, which adds the cases without the change; the end `639f1b0b`, the
  commit of task 3.3. After the count the branch was rebased onto
  `main`'s `7cde76cd`, which moved CI's revision of PostgreSQL and
  changed two suites, no code: the two commits are `bbcb4a6b` and
  `93cf8c8a` there.
- **PostgreSQL** 19rc2, release, without LLVM, with pg_pmu and
  pg_duckdb 1.2.0: the binaries of the count of 2026-11-25, by their
  hashes in the sources below.
- **The families of `bench/pg`**, at scale 1: `bench_dates` and
  `bench_clock` of 2 million rows each. `cargo ab measure --base
  f708ef0d --family <family> --cases <regex> --counters --cycles
  --rounds 2 --deadline 300`, then the same without `--cycles` for the
  instructions. Each side ran twice, in turns with the other, three
  readings a case in a run. `anyagg` with `--cases 'minmax|bool_and'`,
  `wordkey` with `--cases 'time|money|clock|key_date_group'`.
- **ClickBench**: `hits.parquet`, 100 M rows, 14.8 GB, in the page
  cache. `cargo clickbench run --mode on --counters --tries 5 --workers
  0 --queries <n>`, one query a run, five readings after one that warms
  up, their medians. The base and the end in turns, twice: `base`,
  `end`, `base-b`, `end-b` in
  [clickbench-runs.txt](clickbench-runs.txt). pg_duckdb's cycles are
  those of its runs of 2026-11-24, of the same binaries and file, with
  one thread of DuckDB.
- **Machine**: Apple M5 Pro, 18 cores, 64 GB, idle at the maintainer's
  word. The counts of `bench/pg` ran on battery, in the high power mode
  (`pmset` powermode 2), before the charger was connected; ClickBench
  ran on mains. `docs/measuring.md` asks for mains.

The plans of the base and the end are the same in every case and query
but three. `g_minmax_int4_distinct` ran the core's `Aggregate` over a
`Sort` at the base and TessAgg at the end. `key_money_group` grouped
by the core's sorted `Aggregate` at the base and by TessAgg at the end.
`g_group_minmax_int4_distinct` ran TessAgg on both sides, with a table
of the distinct values at the base that wrote to disk, and none at the
end. As the plan says, such a case is judged by the cycles of the whole
query.

## Hypotheses

Written in the design before the runs:

1. `g_minmax_date`, `g_minmax_timestamp`, `g_minmax_int2`,
   `g_bool_and_or`, `g_minmax_clock`: at least 30 per cent fewer
   cycles; the first four within 10 per cent of their twin's cycles at
   the end.
2. `g_group_minmax_dates`: at least 20 per cent fewer cycles, and
   within 10 per cent of its twin's at the end.
3. The two cases of `DISTINCT`: at least 30 per cent fewer cycles, and
   within 5 per cent of their twins' at the end.
4. `key_time_group`, `key_time_distinct`: at least 25 per cent fewer.
5. `key_time_join`: at least 20 per cent fewer.
6. `key_clock_filter`: at least 30 per cent fewer.
7. `key_money_group`: at least 50 per cent fewer.
8. `key_money_order`: at least 10 per cent fewer.
9. The twins `g_minmax_int4`, `g_group_minmax_int4` and the control
   `key_date_group`: within 2 per cent.
10. ClickBench's Q7: 40 to 55 per cent fewer cycles, fewer
    instructions; Q1 and Q3 within 2 per cent of cycles.

## Result of `bench/pg`

Millions of cycles at the base and at the end, each the mean of its two
turns' medians with Tessera on, and the changes of cycles and of
instructions; lower is better. A case marked "again" was counted once
more, as below, and its second count stands after the slash:

| case | base | end | cycles | instructions |
|---|--:|--:|--:|--:|
| g_minmax_date | 83.7 | 43.9 | -47.5 % | -56.8 % |
| g_minmax_timestamp | 78.6 | 44.0 | -44.0 % | -56.2 % |
| g_minmax_int2 | 81.6 | 43.7 | -46.4 % | -56.7 % |
| g_bool_and_or, again | 78.6 | 44.3 | -43.6 / -44.9 % | -56.2 % |
| g_minmax_clock, again | 143.3 | 66.9 | -53.3 / -54.3 % | -62.6 % |
| g_minmax_int4, twin, again | 44.1 | 44.3 | +0.6 / -1.1 % | 0.0 % |
| g_group_minmax_dates | 186.8 | 139.5 | -25.3 % | -32.2 % |
| g_group_minmax_int4, twin | 126.8 | 126.2 | -0.4 % | 0.0 % |
| g_minmax_int4_distinct | 487.3 | 43.6 | -91.0 % | -92.1 % |
| g_group_minmax_int4_distinct | 674.6 | 126.2 | -81.3 % | -73.2 % |
| key_time_group | 201.4 | 96.6 | -52.0 % | -44.5 % |
| key_time_distinct, again | 187.7 | 84.2 | -55.2 / -56.3 % | -48.2 % |
| key_time_join | 206.4 | 94.2 | -54.4 % | -53.5 % |
| key_clock_filter | 121.9 | 49.6 | -59.3 % | -58.0 % |
| key_money_group | 556.6 | 97.3 | -82.5 % | -78.5 % |
| key_money_order | 461.2 | 208.4 | -54.8 % | -42.1 % |
| key_date_group, control, again | 99.5 | 98.1 | -1.4 / -0.2 % | 0.0 % |

The instructions of the twins and of the control moved by less than
0.01 per cent, and those of mode off, the core alone, by 0.1 at most.
At the end the first four cases of hypothesis 1 ran within 1.5 per
cent of their twin `g_minmax_int4`, and the two cases of `DISTINCT`
within 1.6 per cent of theirs.

A side's spread, as the plan defines it, is the difference of its two
turns, which [abturns.sh](abturns.sh) gives from the runs of
[runs.txt](runs.txt). Five cases had a side past 2 per cent and were
counted once more:

- `g_bool_and_or` (3.1 per cent at the end), `g_minmax_clock` (2.6)
  and the twin `g_minmax_int4` (3.3): at most 1.6 the second time,
  settled.
- The control `key_date_group` (2.5 at the base): 1.0 the second time,
  settled.
- `key_time_distinct` (2.4 at the base): 16.6 the second time, so it is
  unsettled by the plan's rule. During the first base turn of that
  count a process of macOS, `knowledgeconstructiond`, took a core: the
  counts of mode off, the core alone, of that run were 9 to 23 per cent
  above those of the other three. Its end spread 0.8 and 1.0 per cent
  in the two counts, and every base turn counted at least 2.1 times the
  end's cycles.

So hypotheses 1 and 3 to 9 hold, `key_time_distinct` unsettled by the
plan's letter. The second fails by half a point: `g_group_minmax_dates`
lost 25.3 per cent of its cycles, past its threshold of 20, but ran
10.5 per cent above its twin at the end, not 10.

## The gap of `g_group_minmax_dates` to its twin

The case computes `min(d)` and `max(ts)`, two columns; its twin
`min(v)` and `max(v)`, one column read once for both. The guess
written before the check: the gap is the second column, not the path
of the dates. [gap.sql](gap.sql) counts on the bench cluster at the
end, with `EXPLAIN (ANALYZE, PMU)`, the case, the twin and three other
pairs, in turns, six times each; [gap.out](gap.out) holds the
readings. Millions of cycles, the median of the five runs after the
first, and the change from the twin:

| query | columns | cycles | to the twin |
|---|---|--:|--:|
| `min(v), max(v)`, the twin | `v` | 127.1 | |
| `min(d), max(d)` | `d` | 130.6 | +2.8 % |
| `min(d), max(v)` | `d`, `v` | 137.8 | +8.4 % |
| `min(v), max(ts)` | `v`, `ts` | 138.4 | +8.9 % |
| `min(d), max(ts)`, the case | `d`, `ts` | 139.0 | +9.4 % |

A second column costs 5 to 9 per cent; with two columns, the dates and
the timestamps run within 1 per cent of a pair with an `integer`. The
guess holds: the twin reads less than the case. The 2.8 per cent of
`min(d), max(d)` over the twin are the same kernels over other data, a
date of 1 826 scrambled values against `v` in the order of the rows,
and were not looked into further.

## Result of ClickBench

Billions of cycles at the base and at the end in the second pair, the
change in each pair, and the end's cycles over pg_duckdb's, with the
base's before the arrow; lower is better:

| query | base | end | change, by pair | over pg_duckdb |
|---|--:|--:|--:|--:|
| Q7 | 4.50 | 2.55 | -43.7, -43.4 % | 5.11 → 2.89 |
| Q3 | 4.30 | 4.29 | -0.3, -0.4 % | 4.45 → 4.43 |
| Q1 | 0.335 | 0.330 | +1.1, -1.5 % | 1.60 → 1.58 |

Q7 is `min` and `max` of `EventDate`; Q3 sums and counts integers on
the node's own path; Q1 reads no column. The instructions of Q7 fell
51.4 and 51.6 per cent, from 33.2 to 16.2 and 16.1 billion; those of
Q1 and Q3 moved by less than 0.01 per cent. Every side spread within 1.5 per cent
between its turns ([turns.sh](turns.sh)). Hypothesis 10 holds: Q7 lost
43 to 44 per cent of its cycles, past the plan's threshold of 30, and
Q1 and Q3 stayed within 2.

## Files

- [runs.txt](runs.txt): the runs of `bench/pg`, by family, measure and
  side, and the directories of `cargo ab`'s reports;
  [clickbench-runs.txt](clickbench-runs.txt): the runs of ClickBench,
  by side and query.
- [abturns.sh](abturns.sh) `<base> <base-b> <end> <end-b>`: the median
  of each case in each run of `bench/pg` and each side's spread.
- [compare.sh](compare.sh) `<side> <base>`: a ClickBench side against
  its base, instructions and cycles, with the spread of each run's
  readings; [turns.sh](turns.sh): each side's cycles by turn and their
  spread; [duckdb.sh](duckdb.sh) `<side>`: a side's cycles over
  pg_duckdb's.
- [gap.sql](gap.sql) and [gap.out](gap.out): the check of the gap of
  `g_group_minmax_dates`.
- `source-ab-base.txt` and `source-ab-end.txt`: the build, the settings
  and the hashes of the libraries of a run of `bench/pg` of each side;
  `source-base.txt`, `source-end.txt` and `source-duckdb.txt`: the same
  of a run of ClickBench of each.

The runs' raw data stay in `target/bench-runs/` of the machine.
