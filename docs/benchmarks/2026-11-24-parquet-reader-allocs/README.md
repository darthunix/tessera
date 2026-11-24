# The Parquet reader's bytes form written in place, 2026-11-24

The Parquet reader's entry of the bytes form, `tess_parquet_bytes`,
gave each ask of a batch, 64 rows of a window of 1024, an array of a
slice for every row of the window, set before any row was read, and a
mask of the rows' `NULL`s, both allocated and freed by the ask. The
change writes each row's bytes into the C side's outputs in place and
keeps the mask in the reader; the reference that holds the window is
still allocated an ask, and the read row by row allocates as before.
A chunk's dictionary of strings, read to rule out a row group, keeps
its values in its page instead of a vector each.

Hypothesis, written before the first run: Q6, Q17 and Q18 1 to 2 per
cent fewer instructions, Q13 under 1, every other query the same.

- **Tessera**, release builds: the base `a3420684`, the end of stage 2
  of `text-keys-by-bytes` before `parquet-fdw` was rebased onto `main`
  and before `agg-distinct-spill` came under it, whose code is that of
  its count's `keys`
  ([its runs](../2026-11-24-text-keys-by-bytes-keys/runs.txt)), so
  that those runs stand for the base's; the first try `92506b3d`,
  `allocs`; the end `1fadbdba`, `allocs2` and `allocs3`, whose library
  is one (`source-allocs2.txt`, `source-allocs3.txt`). Both are commits
  of the branch before its history was put in order: the end's tree
  differs from the branch's commit of the roadmap's entry by the check
  of debug builds, made before any output is written, a field and two
  tests of the tests, comments and the roadmap's entry, and its release
  machine code of `tess_parquet_bytes` is the same.
- **PostgreSQL** 19rc2, release, without LLVM, with pg_pmu.
- **Data**: `hits.parquet`, 100 M rows, 14.8 GB, in the page cache.
- **Method**: `cargo clickbench run --mode on --counters --tries 5
  --workers 0`, one query a run, five readings after one that warms
  up, their medians; instructions decide. Q34 and Q35, whose spill of
  11 GB the kernel's reads count too, with `--tries 1 --timeout 150`,
  the base and the end back to back, `base34` and `allocs34`. The
  heap's families are not counted: the change is the Parquet reader's.
- **Machine**: Apple M5 Pro, 18 cores, 64 GB; on battery, as each
  source records, where the base's runs of stage 2 were on mains. The
  count of instructions does not depend on the clock, which the power
  source sets; cycles do, and are not compared.
- **Not counted**: Q23 and Q29, whose readings pass the timeout.

## Result

Billions of instructions against the base; lower is better:

| query | what | base | end | change |
|---|---|--:|--:|--:|
| Q6 | distinct phrases | 74.789 | 70.369 | -5.9 % |
| Q13 | phrases counted, a filter | 71.660 | 71.189 | -0.7 % |
| Q15 | engine and phrase | 84.399 | 83.895 | -0.6 % |
| Q17 | user and phrase, ordered | 192.835 | 188.969 | -2.0 % |
| Q18 | user and phrase, a limit | 124.858 | 120.405 | -3.6 % |
| Q19 | user, minute and phrase | 428.200 | 426.920 | -0.3 % |
| Q34 | URL, a pair | 346.224 | 345.068 | -0.3 % |
| Q35 | 1 and URL, a pair | 356.927 | 353.236 | -1.0 % |
| Q37 | URL of a counter | 1.806 | 1.776 | -1.7 % |
| Q38 | title of a counter | 1.718 | 1.685 | -1.9 % |
| Q40 | sources and URL | 3.824 | 3.790 | -0.9 % |

The hypothesis held for Q13 and Q17. Q6 and Q18 lost more than it said,
and the other queries of strings lost some where it said they would not
move; how much of that is the array's setting and how much its
allocation was not counted apart.

Every other query within 0.15 per cent, the queries of a `LIKE`, Q21,
Q22 and Q24, among them. Q19, Q15 and Q13 lost the least of the
queries of strings, under 1 per cent; in Q13, the one profiled, half of
the samples of stage 2's profile are the reading of the column.

## What the count found

The first try, `allocs`, took each row's bytes in `fill_rows` through a
closure, and walked the rows through the iterator the function had
used before. With the closure the compiler called that iterator's
`next` out of line, twice a row, once to check and once to write: Q6
2.2 per cent more than the base, Q19 2.1, Q18 1.3, Q17 0.85. The second
try, written before it ran, walks the rows by a loop over the set bits,
as `mark_nulls` does, and its machine code calls nothing a row: the
result above. Every run is kept.

## Files

- [runs.txt](runs.txt): every run, by its side and query.
- [counts.txt](counts.txt): each side against its base, by
  [compare.sh](compare.sh).
- `source-allocs.txt`, `source-allocs2.txt`, `source-allocs3.txt`,
  `source-base34.txt` and `source-allocs34.txt`: the sources of a run
  of each side, its commit, server, settings, files and the hashes of
  the libraries.

The runs' raw data stay in `target/bench-runs/` of the machine.
