# Strings read by views, `parquet-string-views`, 2026-11-24

The count of the OpenSpec change `parquet-string-views`: the Parquet
reader reads a chunk of strings that is not encoded through its
dictionary in every data page as Arrow views, whose values stay in the
decompressed pages, where it read them as `LargeBinary` and copied each
value. The change is kept when Q28 loses at least 8 per cent of its
instructions against the commit the work starts from, Q26 at least 5,
and no query counted gains more than 1 per cent.

- **Tessera**, release builds: the base `2f08e7db`, `main`, `base` in
  [runs.txt](runs.txt); the end `eb0aa25c`, the reviews' fixes,
  `end`, whose code is the change's last.
- **PostgreSQL** 19rc2, release, without LLVM, with pg_pmu.
- **Data**: `hits.parquet`, 100 M rows, 14.8 GB, in the page cache.
- **Method**: `cargo clickbench run --mode on --counters --tries 5
  --workers 0`, one query a run, five readings after one that warms
  up, their medians; instructions decide, cycles are beside them.
  pg_duckdb's cycles are those of the run `duckdb` of the stage of the
  sort by bytes, of the same server and file, with one thread of DuckDB.
- **Q34 and Q35** spill 11 GB and depend on the page cache: counted
  with `--tries 1 --timeout 150`, the base and the end back to back.
- **Machine**: Apple M5 Pro, 18 cores, 64 GB.
- **Not counted**: Q23 and Q29, whose every reading passes the timeout
  of 60 s.

## Hypotheses

Written before any run:

1. Q28, `avg(length(URL))`, runs 10 to 16 per cent fewer
   instructions: the copies of its profile.
2. Q26, `ORDER BY SearchPhrase LIMIT 10`, 6 to 12.
3. Q13 and the other queries that read `SearchPhrase`, `URL`, `Title`
   or `Referer` in full run fewer.
4. The queries that read no string are within 1 per cent.

## Result

Billions of instructions against the base; lower is better; and the
cycles of the end over pg_duckdb's, lower is better:

| query | what | base | end | change | over pg_duckdb |
|---|---|--:|--:|--:|--:|
| Q28 | `avg(length(URL))` | 188.637 | 166.369 | -11.8 % | 2.33 |
| Q26 | the first phrases | 52.303 | 47.000 | -10.1 % | 2.40 |
| Q21 | `URL LIKE '%google%'` | 64.292 | 52.600 | -18.2 % | 0.78 |
| Q22 | phrases with `google` | 151.328 | 135.008 | -10.8 % | 3.23 |
| Q25 | phrases by the time | 40.065 | 35.730 | -10.8 % | 1.21 |
| Q13 | grouped by the phrase | 71.231 | 65.918 | -7.5 % | 2.67 |
| Q34 | grouped by `URL`, a pair | 334.045 | 325.707 | -2.5 % | |
| Q35 | `1` and `URL`, a pair | 343.656 | 333.274 | -3.0 % | |
| Q3 | sums of two numbers | 27.613 | 26.693 | -3.3 % | 4.54 |

No query of the 39 gains: every one but Q1, `COUNT(*)`, which reads no
column, runs fewer instructions, from 0.07 to 18 per cent, and Q2, whose
readings of one build alternate between two counts, 14. The cycles
follow: Q28 falls from 57.8 to 53.3 billion, Q26 from 15.0 to 13.6.
Over the 39 queries the end runs 1.93 times pg_duckdb's cycles by their
geometric mean, the base 2.03 ([counts.txt](counts.txt)).

So hypotheses 1 to 3 hold, Q26 and Q28 past the change's thresholds,
and the fourth does not: queries that read no string lost 1 to 7 per
cent too.

## Why the queries without strings lost

A forward reader kept the rows past a window as a slice of the joined
window, always one array, and joined it with the arrays read for the
next window: a join of two arrays copies their values, so every window
of every column, of any type, was copied once more, the rest empty or
not. The reviews' fixes keep the rest as a slice of the last array
read, and a window that one array holds is a slice of it, joined with
nothing. The base's profile of Q28 shows that join, the reader's
`concat`, copying bytes: 49 of its 2 900 samples.

## Why the first build lost less

The first build of the views checked each value by a search for a zero
byte and `from_utf8`, a call each: Q28 lost 2.0 per cent and Q26 4.9.
Its profile ([profiles-q28.txt](profiles-q28.txt)) had the copies of
the base gone, from 441 samples of `memmove` to 65, and the two calls
at 609 of 2 854 samples, where the base's check of a window in one pass
took 205. One pass a value by words of 8 bytes, `from_utf8` only for a
value with a byte at or above 0x80, took Q28 to -9.7 per cent and Q26
to -5.7 (`try1` in the runs kept on the machine); the reviews' fixes to
the figures above.

## Files

- [runs.txt](runs.txt): every run, by its side and query: `base`,
  `end`, the pairs `base34` and `end34`, and `duckdb`.
- `source-base.txt`, `source-end.txt` and `source-duckdb.txt`: the
  sources of a run of each, its commit, server, settings, files and the
  hashes of the libraries built.
- [counts.txt](counts.txt): each side against its base by
  [compare.sh](compare.sh), the cycles by [cycles.sh](cycles.sh).
- [profiles-q28.txt](profiles-q28.txt): the functions of Q28's
  profiles of the base and of the first build of the views.

The runs' raw data stay in `target/bench-runs/` of the machine.
