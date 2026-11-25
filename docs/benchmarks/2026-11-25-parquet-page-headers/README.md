# Page headers through a buffer, `parquet-page-headers`, 2026-11-25

The count of the OpenSpec change `parquet-page-headers`: the Parquet
reader gives the `parquet` crate a buffer of 1 KiB over its reader of
the file from an offset, so that a page's header, which the crate's
Thrift reader reads mostly a byte at a time, costs one system call
where it cost some 20. Cycles decide, by the maintainer's rule;
instructions stand beside them. The change is kept when Q8 loses at
least 12 per cent of its cycles, Q11 at least 4, and no query counted
gains more than 2.

- **Tessera**, release builds: the base `fc6b0208`, `main`; the end
  `bdaa60ee`, the tree of task 2.1's commit before its message carried
  these counts; and two variants of the end, with buffers of 256 bytes
  and of 8 KiB (`+` in their sources: the one constant changed). The
  code merged differs from the end counted by a later commit of the
  reviews' fixes, in the bytes kept of a header, not counted again.
- **PostgreSQL** 19rc2, release, without LLVM, with pg_pmu.
- **Data**: `hits.parquet`, 100 M rows, 14.8 GB, in the page cache.
- **Method**: `cargo clickbench run --mode on --counters --tries 5
  --workers 0`, one query a run, five readings after one that warms up,
  their medians. The base and the end in turns, twice: `base`, `end`,
  `base-b`, `end-b` in [runs.txt](runs.txt); Q1, Q2, Q3 and Q8 once
  more, `base-c` and `end-c`, as the plan asks of a query whose side
  spread past 2 per cent; then Q8 and Q28 at the two other sizes,
  `b256` and `b8k`. pg_duckdb's cycles are those of the runs `duckdb`
  of the day before, of the same server and file, with one thread of
  DuckDB.
- **Queries**: Q8, which groups the rows of `AdvEngineID <> 0` by
  `AdvEngineID`, and Q2, which counts them; Q11, which skips the pages
  of `MobilePhoneModel`; Q28 and Q26, whose pages are read whole; Q3,
  which reads numbers; Q1, which reads no page, the control.
- **Machine**: Apple M5 Pro, 18 cores, 64 GB, on mains power, idle at
  the maintainer's word.

The plans of the base and the end are the same for every query, each
with the reader's scan, `TessParquetScan`.

## Hypotheses

Written before the runs:

1. Q8: 15 to 23 per cent fewer cycles, its headers' system calls, of
   24 per cent; fewer instructions too.
2. Q2: as Q8, within 5 points; not profiled.
3. Q11: 5 to 12 per cent fewer cycles, of `pread`'s 12 per cent.
4. Q28, Q26, Q3, Q1: within 2 per cent of cycles.

## Result

Billions of cycles at the base and at the end in the second pair, the
change in each pair, and the end's cycles over pg_duckdb's, with the
base's before the arrow; lower is better. The queries are those of the
list above:

| query | base | end | change, by pair | over pg_duckdb |
|---|--:|--:|--:|--:|
| Q8 | 1.49 | 1.13 | -18.8, -24.2, -20.9 % | 3.37 → 2.56 |
| Q2 | 0.469 | 0.435 | -5.2, -7.2, -3.3 % | 1.08 → 1.00 |
| Q11 | 6.73 | 6.25 | -8.0, -7.1 % | 3.69 → 3.43 |
| Q28 | 39.9 | 39.3 | -1.7, -1.6 % | 1.75 → 1.72 |
| Q26 | 11.8 | 11.7 | -1.7, -0.9 % | 2.08 → 2.06 |
| Q3 | 4.35 | 4.31 | -0.9, -0.9, -0.3 % | 4.50 → 4.46 |
| Q1 | 0.334 | 0.343 | +0.1, +2.7, +1.5 % | 1.60 → 1.64 |

The instructions fell 15.6 to 15.7 per cent on Q8, 6.8 on Q11,
1.7 to 2.2 on Q2, 1.5 on Q28, 1.0 on Q26 and 0.4 to 0.6 on Q3, and
stayed within 0.05 on Q1.

A side's spread, as the plan defines it, is the difference of its
turns, which [turns.sh](turns.sh) gives; [compare.sh](compare.sh) gives
the spread of a run's five readings beside it. The base and the end
spread 0.3 to 0.6 per cent between their turns on Q11, Q26 and Q28. On
the others a side spread past 2 per cent, and they were counted once
more; still past it, they are unsettled by the plan's rule:

- Q8: 3.7 per cent at the base, 3.3 at the end, over three turns, the
  second base and the first end the highest. Each pair lost 18.8 to 24.2
  per cent of its cycles, five times the spread and past the threshold
  of 12, with 15.6 to 15.7 per cent fewer instructions.
- Q2: 5.4 and 6.2 per cent; fewer cycles in each pair, 3.3 to 7.2.
- Q3: 7.3 and 6.9 per cent, from the first turn, which ran after the
  builds: 4.65 against 4.33 to 4.35 at the base. Its pairs moved -0.9,
  -0.9 and -0.3 per cent.
- Q1: 6.0 and 7.5 per cent: it runs a tenth of a second and reads no
  page, and its instructions are the same to 0.05 per cent. Its moves
  are below its spread.

So hypotheses 1, 3 and 4 hold, Q8's second pair a point past its range.
The second fails: Q2 lost 3 to 7 per cent of its cycles, not as much as
Q8. It reads the same column by the same clause in a third of Q8's
cycles, and its saving, 0.03 billion cycles against Q8's 0.33, says it
reads about a tenth as many headers; why was not looked into. Q11
passes its threshold, 8.0 and 7.1 per cent, and no query gained more
than 2 but Q1, +2.7 in a pair, within its spread. Q8 passes its
threshold in every pair but is unsettled by the plan's letter: whether
a change five times the spread counts is the maintainer's to decide.

## The size of the buffer

Q8 and Q28 at buffers of 256 bytes and 8 KiB, against the end in each
pair, the change of cycles; the instructions moved within 0.4 per cent:

| buffer | Q8 | Q28 |
|---|--:|--:|
| 256 bytes | -0.5, +2.8 % | +0.7, +0.3 % |
| 8 KiB | +0.5, +3.8 % | +1.3, +1.0 % |

Neither gives Q8 2 per cent fewer cycles, and on Q8 both sides spread
past it; the size stays 1 KiB, as the plan says.

## Files

- [runs.txt](runs.txt): every run, by its side and query.
- [compare.sh](compare.sh): a side against its base, instructions and
  cycles, with the spread of each run's readings;
  [turns.sh](turns.sh): each side's cycles by turn and their spread, as
  the plan defines it; [duckdb.sh](duckdb.sh): a side's cycles over
  pg_duckdb's.
- `source-base.txt`, `source-end.txt`, `source-256.txt`,
  `source-8k.txt` and `source-duckdb.txt`: the sources of a run of
  each, its commit, server, settings, files and the hashes of the
  libraries built.

The runs' raw data stay in `target/bench-runs/` of the machine.
