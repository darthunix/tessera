# Batch arrays kept for the scan, `parquet-batch-arrays`, 2026-11-27

The count of the OpenSpec change `parquet-batch-arrays`. The Parquet
node allocated and zeroed, at each batch of up to 64 rows, three arrays
of an entry for each attribute of the foreign table, 2 520 bytes for
the 105 attributes of `hits`, and the arrays of each column asked. It
now keeps the arrays for the scan, for the columns it reads, zeroed
once, and clears at each batch the rows made and the views'
dictionaries alone. Cycles decide, by the maintainer's rule;
instructions stand beside them.

- **Tessera**, release builds: the base `201f2bee`, `main` where the
  branch starts; the end `f297c96e`, the commit of task 2.1. Each in a
  worktree of its own, built clean for the server below.
- **PostgreSQL** 19rc2, release, without LLVM, with pg_pmu and
  pg_duckdb 1.2.0: the binaries of the counts of 2026-11-24 to
  2026-11-26, by their hashes in the sources below.
- **ClickBench**: `hits.parquet`, 100 M rows, 14.8 GB, in the page
  cache. `cargo clickbench run --mode on --counters --tries 5 --workers
  0 --queries <n> --files hits.parquet`, one query a run, five
  readings after one that warms up, their medians. The base and the
  end in turns, query by query: `base`, `end`, then `base-b`, `end-b`
  in [clickbench-runs.txt](clickbench-runs.txt); Q1 a third time,
  `base-c`, `end-c`, as the plan asks of a side that spreads past 2
  per cent. The control, Q1 with `--mode off`, in the same turns,
  `base-off` and `base-b-off` and the end's. pg_duckdb's cycles are
  those of its runs of 2026-11-24, of the same binaries and file, with
  one thread of DuckDB.
- **Machine**: Apple M5 Pro, 18 cores, 64 GB, idle at the maintainer's
  word, on mains. The script waited for a minute in which no process
  but the agent's took a fifth of a core: `mediaanalysisd` took a core
  for half a minute before the first run.

The plans of the base and the end are the same in every query, Tessera
on and off.

## Hypotheses

Written in the design before the runs:

1. Q1, `COUNT(*)`, which reads no column: 30 to 55 per cent fewer
   cycles, its profile's half; fewer instructions.
2. Q3, `SUM`, `COUNT` and `AVG` over two integer columns: 5 to 12 per
   cent fewer cycles, its profile's tenth.
3. Q7, the least and the greatest of one date column: 7 to 15 per cent
   fewer cycles, not profiled, from the savings a batch of Q1 and of
   one column of Q3.
4. Q28, an average of a string's length by groups, the string in the
   bytes form: within 2 per cent, a guard.
5. The control, Q1 with Tessera off: within 2 per cent.

The change is kept when Q1 loses at least 25 per cent of its cycles,
Q3 at least 4, and no query gains more than 2. A side's spread is the
difference of its turns; a change smaller than the larger spread, or
than the control's move, is no change; a side past 2 per cent is
counted once more, and still past it the query is unsettled, which the
maintainer decides.

## Result

Billions of cycles at the base and at the end in the first pair, the
change in each pair, and each side's spread over its turns; lower is
better:

| query | base | end | change, by pair | spread, base / end |
|---|--:|--:|--:|--:|
| Q1 | 0.336 | 0.153 | -54.4, -55.8, -58.2 % | 11.1 / 5.4 % |
| Q3 | 4.282 | 3.990 | -6.8, -6.2 % | 0.4 / 0.3 % |
| Q7 | 2.554 | 2.290 | -10.3, -10.6 % | 0.6 / 0.8 % |
| Q28 | 39.13 | 38.54 | -1.5, -1.8 % | 0.3 / 0.0 % |
| Q1, off | 23.93 | 23.87 | -0.2, +0.0 % | 0.0 / 0.2 % |

Instructions, which repeat from pair to pair within 0.02 per cent but
the base's of Q3 and Q7, 0.3 and 0.5:

- Q1 1.879 to 1.115 billion, -40.7 per cent;
- Q3 26.6 to 25.1, -5.7 and -5.5;
- Q7 16.1 to 15.0, -6.8 and -7.3;
- Q28 156.3 to 154.2, -1.3;
- the control 123.71 on both sides.

The end's cycles over pg_duckdb's, with the base's before the arrow:
Q1 1.61 → 0.70 to 0.73, Q3 4.43 → 4.13, Q7 2.90 → 2.58 to 2.60, Q28
1.71 → 1.69.

How the hypotheses came out:

1. Q1 lost 54 to 58 per cent of its cycles, past the threshold of 25,
   and 41 per cent of its instructions. Two pairs of three fall past
   the hypothesis's 55: the profile at the base put the allocations
   and the zeroing at 56 per cent, above the roadmap's half, which the
   hypothesis took. By the plan's rule Q1 is unsettled: its base
   spreads 11 per cent over three turns, its end 5. The third pair's
   base read 14 per cent apart, and macOS's `XprotectService` took a
   fifth of a core when the pair ended; the base's first two turns
   agree to 0.6 per cent. The maintainer decides.
2. Q3 lost 6.2 to 6.8 per cent of its cycles, past the threshold of 4,
   within the hypothesis.
3. Q7 lost 10.3 to 10.6, within the hypothesis.
4. Q28 lost 1.5 to 1.8, within the guard's 2, fewer cycles.
5. The control moved 0.2 per cent and less, its instructions not at
   all.

No query gained cycles. The change is kept by the plan's thresholds,
with Q1 unsettled by its spread.

## Profiles

`sample` of the backend running the query again and again, 4 s, once
a millisecond, at the base and at the end; the functions on top of the
stacks in [profile-q1-base.txt](profile-q1-base.txt),
[profile-q1-end.txt](profile-q1-end.txt),
[profile-q3-base.txt](profile-q3-base.txt) and
[profile-q3-end.txt](profile-q3-end.txt). The samples in `memset`,
`bzero`, `MemoryContextAllocZero`, `AllocSetAlloc`,
`MemoryContextReset`, `AllocSetReset` and their stubs:

- Q1: 1 877 of 3 366 at the base, 55.8 per cent, `memset` alone 922;
  243 of 3 374 at the end, 7.2 per cent, of which 203 the calls of
  `MemoryContextReset` on the batch's empty context;
- Q3: 261 of 3 266 at the base, 8.0 per cent; 73 of 3 266 at the end,
  2.2.

At the end Q1's time goes to the node's `next_batch`, 13 per cent,
TessAgg's `agg_exec`, 12, and the publishing and taking of each batch,
`tess_output_publish`, `tess_input_next`, `tess_output_release` and
`publish_batch`, 25 together. Q3's goes to `datum-arrow`'s read of the
integers, 24 per cent, the sums, and the node's `batch_column`, 14,
which the entry `parquet-integer-columns` holds.

## Files

- [clickbench-runs.txt](clickbench-runs.txt): the runs, each side's
  turn and query and its directory under the worktree's
  `target/bench-runs`.
- [turns.sh](turns.sh) `[-off]`: each side's medians by turn and its
  spread; [compare.sh](compare.sh) `<side> <base>`: a pair's
  instructions and cycles and the spread of each run's readings;
  [duckdb.sh](duckdb.sh) `<side>`: a side's cycles over pg_duckdb's.
- `profile-q1-*.txt`, `profile-q3-*.txt`: the profiles above.
- `source-base.txt`, `source-end.txt` and `source-duckdb.txt`: the
  revision, the settings and the hashes of the binaries of a run of
  each.
