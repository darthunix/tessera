# The bytes form, stage 1 of `text-keys-by-bytes`, 2026-11-23

Stage 1 of the measurement plan of the OpenSpec change
`text-keys-by-bytes`: the bytes form in the batch contract, and the
batches of the Parquet source offering it (groups 2 and 3 of its
tasks). No consumer reads the form yet, so every query is a control:
the stage is kept when no query counted gains more than 1 per cent of
instructions against the commit the stage starts from.

- **Tessera**, release builds: the base `531b5d7d`, the commit of
  `parquet-fdw` the change starts from; the first end `f75a103f`; the
  end at `3c7b5f77`, and its Q34 and Q35 at `94060934`, the column C.
  Those are the stage's last commits before their messages were
  reworded; the branch holds them as `03fd9251` to `b406b9fb`, whose
  tree is `94060934`'s, and `3c7b5f77` differs from it only by the name
  of the macro that inlines a function, the same macro on
  PostgreSQL 19. The plan's second base, stage 4 of `parquet-fdw`, is
  the same code: that change's task of it, 6.3, counts the queries as
  `531b5d7d` leaves them, so for this first stage the two bases are
  one.
- **PostgreSQL** 19rc2, release, without LLVM, with pg_pmu: a copy of
  the build of stage 2 of `parquet-fdw`.
- **Data**: `hits.parquet`, 100 M rows, 14.8 GB, in the page cache.
- **Method**: `cargo clickbench run --mode on --counters --tries 5
  --workers 0`, one query a run, five readings after one that warms up,
  their medians; instructions decide, cycles are beside them. The
  heap's cases of strings by `cargo ab measure --counters --base
  531b5d7d --family sort --family anykey`. By the maintainer's word a
  count needs no idle machine: other sessions built and tested beside
  the runs.
- **Machine**: Apple M5 Pro, 18 cores, 64 GB.
- **Not counted**: Q23 and Q29, whose every reading passed the timeout
  of 60 s in both builds. The end's first runs of Q34 and Q35 timed out
  in their warm-up while the suites ran beside them; they are kept in
  [runs.txt](runs.txt) as `timeout`, and the queries were counted again
  at the end's last commit, the column C of the end in
  [counts.txt](counts.txt), whose report therefore gives no sums.

## Result

Every query counted is within 1 per cent of the base: the most, Q3,
+0.85 per cent, Q1 +0.38, Q4 +0.30; every other within 0.3 either
way. Of the 41 queries counted, taking a change within 0.1 per cent as
none, 6 run fewer instructions, 30 the same and 5 more; the median
change is +0.00 per cent. The heap: all 33 cases of `sort` and
`anykey` the same, the median +0.002 per cent and the most +0.04, the
controls from 0.999 to 1.001.

## What the first count found

The first end, `f75a103f`, took Q1, `count(*)`, from 1.862 to 2.101
billion instructions, +12.8 per cent, and Q3, Q4, Q5 and Q7 from +1.1
to +2.2. Each try was a hypothesis written before it ran:

1. The batch's array of the columns' bytes, made and zeroed for every
   batch, 105 pointers for `hits`, about 150 instructions a batch over
   1.56 M batches, is Q1's excess. Made at the batch's first ask of the
   form instead, Q1 fell to +0.3 per cent; Q3 stayed at +1.2. Kept.
2. The window's read, which 3.3 moved out of the making of `Datum`
   values to share it with the bytes form, became a call of its own
   each ask. Made always inline, Q3 fell to +0.9, the others within
   0.4. Kept.

Billions of instructions; lower is better:

| query | what | base | first | 1 | 2 | end |
|---|---|--:|--:|--:|--:|--:|
| Q1 | `count(*)` | 1.862 | 2.101 | 1.868 | 1.868 | 1.869 |
| Q3 | sums of two columns | 27.274 | 27.823 | 27.604 | 27.524 | 27.507 |
| Q4 | avg of `UserID` | 14.532 | 14.852 | 14.618 | 14.574 | 14.575 |
| Q5 | distinct `UserID` | 29.223 | 29.546 | 29.307 | 29.269 | 29.263 |
| Q7 | min, max of a date | 33.605 | 34.001 | 33.760 | 33.728 | 33.643 |

Both fixes are in 3.3's commit, which carries these counts.

## Files

- [runs.txt](runs.txt): every run, by its side and query: `base`,
  `head` (the first end), `lazy` (hypothesis 1), `inline` (2), `end`
  and `timeout`.
- `source-base.txt`, `source-first.txt`, `source-end.txt` and
  `source-end-c.txt`: the sources of a run of each, its commit, server,
  settings, files and the hashes of the libraries built.
- [counts.txt](counts.txt): each side against the base, by
  [report.sh](report.sh), and the report of `cargo ab`.

The runs' raw data stay in `target/bench-runs/` of the machine.
