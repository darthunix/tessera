# Aggregates past 64, `agg-folded-arguments` stage 2, 2026-11-28

The count of the second stage of the OpenSpec change
`agg-folded-arguments`. A group's record keeps a word of flags for each
64 of its aggregates, in the node and in the merge of the hash table's
records, so `TessAgg` takes queries of more than 64 aggregates, which
the first stage left to the core's aggregate. Two bounds take the place
of the cap: at most 1 664 computed columns, and a group's record within
the table's first chunk of 64 kB. Cycles decide, by the maintainer's
rule; instructions stand beside them, and times stand for the cases
whose plans differ.

- **Tessera**, release builds: the base `a71b632a`, the first stage's
  end as `main` holds it after its pull request; the end `68e233db`,
  tasks 4.1 to 4.3 and the cases of more than 64 aggregates in the
  family. `cargo ab` builds both sides in its own directories, and
  takes the family from the branch for both, so the base runs the new
  cases too; ClickBench's base is a worktree at `a71b632a`, its end the
  branch's worktree, both built clean for the server below.
- **The commits' ids** are those of the branch when it was counted. A
  merge by rebase gives the same trees other ids in `main`: the end is
  the commit "agg-folded-arguments: the cases of more than 64
  aggregates".
- **PostgreSQL** 19rc2, release, without LLVM, with pg_pmu: the
  binaries of the first stage's count, by their hashes in the sources
  ([source-base.txt](source-base.txt),
  [source-end.txt](source-end.txt)).
- **`aggfold`**: the family's table of 2 million rows. The cases of
  more than 64 aggregates by `cargo ab measure --base a71b632a --family
  aggfold --cases <case> --repetitions 5 --deadline 300`, two rounds,
  one for the cases of 200; the cases of 64 or fewer by `--counters
  --cycles --rounds 2`, then `--counters` alone for the instructions,
  in the nine groups of the first stage.
- **ClickBench**: `hits.parquet`, 100 M rows, in the page cache, `cargo
  clickbench run --mode <on|off> --counters --tries 5 --workers 0
  --queries <n> --files hits.parquet`, one query a run, the medians of
  five readings after one that warms up; base and end in turns, query
  by query, three pairs of Q3 and Q4 and two of the control
  ([clickbench-runs.txt](clickbench-runs.txt)).
- **Machine**: Apple M5 Pro, 18 cores, 64 GB, idle at the maintainer's
  word, on mains, under `caffeinate -i`. From 09:08 another agent's
  test under Miri held one core to the end of the count. At the
  maintainer's word the check of a quiet minute before each run left
  that process aside: it takes one core of its own, and a count is of
  the backend's own core.

## What went wrong on the way

- The first chain waited for a quiet minute while Miri held its core,
  and was stopped at 09:23. Its two groups of cycles (`ab-NGe6fC`,
  `ab-F0c5gi`) are kept in their directories and left out of the
  tables; the series was counted again whole.
- The first pair of ClickBench's Q3 and Q4 ran in the processor's
  unstable state: the cycles of one run rose from try to try, 3.48 to
  3.72 billion for Q3 at the end and fell 2.16 to 2.01 for Q4 at the
  base, with the instructions unchanged. A third pair was counted, as
  the design's rule asks of a side past 2 per cent.
- `fold_ten_numeric_m1m` and its twin moved 15 and 9 per cent by their
  medians in the series, past the control's 8; a third pair settled
  them (below).

## Hypotheses

Written in the design before the runs:

6. The cases of 65 and 200 aggregates: at most half the time of the
   base, the core's.
7. The cases of 64 or fewer of both stages, Q3 and Q4: within 2 per
   cent of the first stage's end.

A stage is kept when hypotheses 6 and 7 hold and no case takes more
than 2 per cent more cycles or time than its base.

## Result: more than 64 aggregates

`sum(i / k)` for `k` from 1 to 65 or 200: no form folds them, so each
keeps a state of its own. Milliseconds, medians
([aggfold-times.md](aggfold-times.md)); the base runs the core's
aggregate, the end `TessAgg`; lower is better:

The cases: `many_65` and `many_200`, 65 and 200 sums without groups;
`many_65_k37` and `many_200_k37`, the same in 37 groups;
`many_65_spill`, 65 sums in 100 000 groups past a `work_mem` of 4 MB.

| case | base | end | end / base |
|---|--:|--:|--:|
| `many_65` | 437.1 | 269.6 | 0.62 |
| `many_200` | 1 461.8 | 880.9 | 0.60 |
| `many_65_k37` | 514.2 | 427.8 | 0.83 |
| `many_200_k37` | 1 625.5 | 1 299.7 | 0.80 |
| `many_65_spill` | 1 036.2 | 1 920.4 | 1.85 |

Without a spill the node takes 0.60 to 0.83 of the core's time, not the
half the hypothesis named: the arguments, 65 or 200 divisions a row,
are computed by the same expressions on both sides, and they take most
of the time.

`many_65_spill` takes 1.85 of the core's time. Its groups are cold:
100 000 groups of 20 rows each, a group's rows 100 000 rows apart. The
node spills a partition's records whole, each a group's key, its words
of flags and its 65 states, some 550 bytes, and a partition that went
to disk makes new records of its next rows. Between two evictions a
group gets one row or none, so nearly every row of the input becomes a
record on disk: 1 045 541 kB written in 658 564 evictions, where the
core writes the input rows of its new groups, a key and an argument,
103 432 kB. The plan is the planner's choice: its costs count those
bytes, and weigh them too lightly against the core's.

A probe at the end ([probe-spill.txt](probe-spill.txt)), the same query
of 16, 64 and 65 sums, `TessAgg` against the core, shows the loss
predates this stage: 64 sums, which the first stage took too, lose as
65 do. Milliseconds, medians of three:

| sums | `TessAgg` | core | ratio | written, `TessAgg` | written, core |
|--:|--:|--:|--:|--:|--:|
| 16 | 355.9 | 451.4 | 0.79 | 267 721 kB | 61 088 kB |
| 64 | 1 869.5 | 1 097.5 | 1.70 | 1 027 792 kB | 103 176 kB |
| 65 | 2 037.5 | 1 110.5 | 1.83 | 1 045 541 kB | 103 432 kB |

The maintainer decided against calibrating the costs and put the spill
of rows by the input's columns, for wide records of cold groups, into
the roadmap as an entry of its own, `agg-wide-record-spill`: the node's
spill of rows today writes the computed keys and arguments, as wide as
the record.

## Result: 64 aggregates or fewer

`end / base` by the medians of two rounds; the control is the same with
Tessera off; lower is better
([aggfold-cycles.md](aggfold-cycles.md),
[aggfold-instructions.md](aggfold-instructions.md)):

| case | cycles | control | instructions |
|---|--:|--:|--:|
| `fold_two_int4` | 1.000 | 0.995 | 0.999 |
| `twin_two_int4` | 0.991 | 0.998 | 0.999 |
| `fold_ten_int2` | 1.000 | 0.997 | 0.998 |
| `twin_ten_int2` | 1.003 | 0.999 | 0.999 |
| `fold_ten_int4` | 0.986 | 0.995 | 0.999 |
| `twin_ten_int4` | 0.997 | 0.987 | 0.999 |
| `fold_ten_int8` | 1.005 | 0.995 | 1.000 |
| `twin_ten_int8` | 0.996 | 0.995 | 0.999 |
| `fold_ten_numeric` | 1.004 | 1.008 | 1.000 |
| `twin_ten_numeric` | 1.009 | 1.007 | 1.000 |
| `fold_kinds_int4` | 1.008 | 1.003 | 0.999 |
| `twin_kinds_int4` | 1.006 | 0.999 | 0.999 |
| `fold_ten_abs` | 1.004 | 0.995 | 0.999 |
| `twin_ten_abs` | 0.997 | 0.990 | 0.998 |
| `fold_ten_int4_k37` | 0.982 | 1.004 | 1.000 |
| `twin_ten_int4_k37` | 0.984 | 0.991 | 1.000 |
| `fold_ten_int4_m1m` | 1.008 | 0.994 | 1.007 |
| `twin_ten_int4_m1m` | 1.014 | 1.000 | 1.017 |
| `fold_ten_numeric_m1m` | 0.974 | 0.992 | 1.000 |
| `twin_ten_numeric_m1m` | 1.016 | 0.999 | 1.001 |
| `fold_distinct_int4_k37` | 1.002 | 1.000 | 1.000 |
| `twin_distinct_int4_k37` | 1.009 | 1.000 | 1.000 |
| `fold_mixed` | 1.015 | 0.990 | 1.000 |
| `fold_ten_int4_spill` | 0.987 | 1.018 | 1.007 |
| `twin_ten_int4_spill` | 0.965 | 0.999 | 1.017 |
| `guard_lone` | 0.983 | 1.003 | 0.999 |
| `guard_tie` | 0.976 | 1.019 | 0.999 |
| `guard_float` | 0.999 | 1.002 | 1.000 |
| `guard_parallel` | 0.971 | 0.969 | 0.999 |

The cycles of the numeric cases of a million groups are those of their
third pair (`ab-Weleyg`).

The cases of a million groups, spilled past `work_mem`, take 0.7 and
1.7 per cent more instructions, the twins' ten states more than the
folded cases' two: the merge of the records read back from disk
computes the place of each aggregate's flag and state from its word,
some five instructions more an aggregate, as the machine code of the
merge's loop showed before the count. Their cycles did not move past
the control. `fold_mixed`'s 1.5 per cent of cycles, with its
instructions unchanged, is within the 2 per cent.

## Result: ClickBench

Q3 is `SUM`, `COUNT(*)` and `AVG` of two columns, Q4 `AVG(UserID)`;
Q1 with Tessera off is the control. Billions of cycles at the base and
at the end, the change of cycles and of instructions; lower is better:

| query | pair | base | end | cycles | instructions |
|---|---|--:|--:|--:|--:|
| Q3 | first, unstable | 3.226 | 3.643 | +12.9 % | +0.42 % |
| Q3 | second | 3.211 | 3.224 | +0.4 % | +0.33 % |
| Q3 | third | 3.190 | 3.184 | -0.2 % | +0.00 % |
| Q4 | first, unstable | 2.089 | 1.807 | -13.5 % | +0.06 % |
| Q4 | second | 1.857 | 1.808 | -2.6 % | +0.02 % |
| Q4 | third | 1.806 | 1.849 | +2.4 % | +0.03 % |
| Q1, off | first | 23.946 | 23.956 | +0.0 % | -0.00 % |
| Q1, off | second | 23.797 | 23.872 | +0.3 % | -0.00 % |

Q4's third pair, +2.4 per cent, is within the spread of each side
between its stable pairs, 2.8 per cent at the base and 2.3 at the end;
its instructions did not move.

## Verdict

Hypothesis 7 holds: no case of 64 aggregates or fewer, and neither Q3
nor Q4, moved past 2 per cent by the design's rule of judging, and the
instructions moved by 1.7 per cent at most, in the merge of spilled
records. Hypothesis 6 holds in its direction alone: without a spill the
node takes 0.60 to 0.83 of the core's time, not half; with a spill of
cold groups, 1.85, a loss that predates this stage and that the
maintainer put into the roadmap. By its own rule the stage is kept
only by the maintainer's decision.
