# Integers converted for the rows asked, `parquet-integer-columns`, 2026-11-27

The count of the OpenSpec change `parquet-integer-columns`. The dense
loops of `datum-arrow`'s read converted every slot of the Parquet
node's window, up to 1 024 rows, for each batch of 64 rows it asked:
16 times each value of a full window. They now convert the words of the
mask from the first that asks a slot to the last, and the reader keeps
its mask of `NULL` rows from call to call. Cycles decide, by the
maintainer's rule; instructions stand beside them, and times stand for
the claim of speed.

- **Tessera**, release builds: the base `119e8c93`, `main` where the
  branch starts; the end `14f4861b`, the commit of task 2.2. Each in a
  worktree of its own, built clean for the server below.
- **PostgreSQL** 19rc2, release, without LLVM, with pg_pmu and
  pg_duckdb 1.2.0: the binaries of the counts of 2026-11-24 to
  2026-11-27, by their hashes in the sources below.
- **ClickBench**: `hits.parquet`, 100 M rows, 14.8 GB, in the page
  cache. `cargo clickbench run --mode <on|off> --counters --tries 5
  --workers 0 --queries <n> --files hits.parquet`, one query a run, five
  readings after one that warms up, their medians. The base and the end
  in turns, query by query: `base`, `end`, then `base-b`, `end-b` in
  [clickbench-runs.txt](clickbench-runs.txt); Q3 with Tessera off a
  third time, `base-c-off` and `end-c-off`, as the plan asks of a side
  that spreads past 2 per cent. The times, `cargo clickbench run --mode
  on --workers 0 --queries <n>` without counters, three tries, after
  the counts: `time-base` and `time-end`. pg_duckdb's cycles are those
  of its runs of 2026-11-24, of the same binaries and file, with one
  thread of DuckDB.
- **Machine**: Apple M5 Pro, 18 cores, 64 GB, idle at the maintainer's
  word, on mains. Another session's random queries ran on the machine
  during part of the count: the runs of Tessera off from 13:53 to 14:01
  started at a load average of 3 to 8 over a minute, where the others
  started at 1.5 to 3 (`source.txt` of each run); the script of the
  third pair waited for them to end.

Before the counts, the machine code of the dense loops of `datum-arrow`
at the base and at the end, by `cargo rustc --release -p datum-arrow
--lib -- --emit asm`: the same counts of widening (`sshll`, 661),
comparing (`cmgt`, 466) and folding (509) vector instructions, and the
same first widening loop of a word, 16 values a step. The plans of the
base and the end are the same in every query, Tessera on and off.

## Hypotheses

Written in the design before the runs:

1. Q3, `SUM`, `COUNT` and `AVG` over two integer columns: 18 to 27 per
   cent fewer cycles, 15 sixteenths of `ReadPlan::read`'s 24.3 per cent
   and the mask's 2.2; fewer instructions.
2. Q4, `AVG` of a 64-bit integer column: 10 to 22 per cent fewer
   cycles, not profiled.
3. Q7, `MIN` and `MAX` of a date column: 12 to 25 per cent fewer cycles,
   not profiled.
4. Q28, a string's length by groups in the bytes form, a guard; Q3 with
   Tessera off, the wrapper's whole windows, a guard; Q1 with Tessera
   off, the control: within 2 per cent.

The change is kept when Q3 loses at least 12 per cent of its cycles and
no query gains more than 2. A side's spread is the difference of its
turns; a side past 2 per cent is counted once more, and still past it
the query is unsettled, which the maintainer decides.

## Result

Billions of cycles at the base and at the end in the first pair, the
change in each pair, and each side's spread over its turns; lower is
better:

| query | base | end | change, by pair | spread, base / end |
|---|--:|--:|--:|--:|
| Q3 | 3.952 | 3.174 | -19.7, -19.9 % | 0.7 / 0.5 % |
| Q4 | 2.208 | 1.797 | -18.6, -18.3 % | 0.1 / 0.3 % |
| Q7 | 2.282 | 1.474 | -35.4, -35.7 % | 0.0 / 0.5 % |
| Q28 | 38.56 | 38.49 | -0.2, -1.2 % | 0.6 / 0.3 % |
| Q3, off | 28.33 | 28.37 | +0.2, -2.3, +0.7 % | 5.5 / 2.9 % |
| Q1, off | 23.97 | 23.83 | -0.6, -0.2 % | 0.2 / 0.1 % |

Instructions, which repeat from pair to pair within 0.5 per cent:

- Q3 25.2 to 20.0 billion, -20.6 and -20.3 per cent;
- Q4 12.83 to 10.93, -14.8;
- Q7 14.97 to 9.40, -37.2;
- Q28 154.2 to 151.8, -1.5;
- Q3 with Tessera off 151.55 and 151.53, the same to 0.06 per cent in
  each pair; the control 123.71 on both sides.

Hot times, the best of three tries, in seconds:

- Q3 0.932 → 0.775, -16.8 per cent;
- Q4 0.534 → 0.451, -15.5;
- Q7 0.551 → 0.358, -35.0.

The end's cycles over pg_duckdb's, with the base's before the arrow: Q3
4.09 → 3.28, Q4 0.74 → 0.60, Q7 2.59 → 1.67, Q28 1.69 → 1.69.

How the hypotheses came out:

1. Q3 lost 19.7 and 19.9 per cent of its cycles, past the threshold of
   12, within the hypothesis; 20 per cent of its instructions.
2. Q4 lost 18.3 and 18.6, within the hypothesis.
3. Q7 lost 35.4 and 35.7, past the hypothesis's 25, and 37 per cent of
   its instructions. The hypothesis took the cost of a column from Q3's
   integers; a date's conversion shifts the epoch and checks its range
   too, so its 15 sixteenths were a larger share of Q7.
4. Q28 lost 0.2 and 1.2, its instructions 1.5: its integer column of
   groups, `CounterID`, goes through the same loop. The control moved
   0.6 and 0.2. Q3 with Tessera off moved +0.2, -2.3 and +0.7 per cent,
   its instructions not at all, and is unsettled by the plan's spread:
   its base spread 5.5 per cent over three turns, its end 2.9, with the
   second pair's readings 5.7 and 6.8 per cent apart while another
   session's queries ran.

No query gained past 2 per cent in any pair. The change is kept by the
plan's thresholds, with Q3 with Tessera off unsettled by its spread.

## Profiles

`sample` of the backend running Q3 again and again, 4 s, once a
millisecond, at the base and at the end; the functions on top of the
stacks in [profile-q3-base.txt](profile-q3-base.txt) and
[profile-q3-end.txt](profile-q3-end.txt), and by the call graph:

- `ReadPlan::read`, the conversion, 784 of 3 217 samples on top at the
  base, 24.4 per cent; at the end its dense loop, `read_plain`, 198 of
  3 239, 6.1.
- The frees of the mask of `NULL` rows under `tess_parquet_values`, 68
  samples at the base, none at the end. The frees and the `memset` left
  belong to the reading of a window, its Arrow arrays.
- At the end the node's copy of a batch's values from the window into
  its arrays, `batch_column` on top, is 644 samples, 19.9 per cent, the
  most of any function; the sums, `sum_terms`, 604, 18.6; the reading
  of a window, `tess_parquet_read` with its decoding, about 30 by the
  call graph.

## Files

- [clickbench-runs.txt](clickbench-runs.txt): the runs, each side's
  turn and query and its directory under the worktree's
  `target/bench-runs`.
- [turns.sh](turns.sh) `[-off]`: each side's medians by turn and its
  spread; [compare.sh](compare.sh) `<side> <base>`: a pair's
  instructions and cycles and the spread of each run's readings;
  [duckdb.sh](duckdb.sh) `<side>`: a side's cycles over pg_duckdb's.
- `profile-q3-base.txt`, `profile-q3-end.txt`: the profiles above.
- `source-base.txt`, `source-end.txt` and `source-duckdb.txt`: the
  revision, the settings and the hashes of the binaries of a run of
  each.
