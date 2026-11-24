# The sort by bytes, stage 4 of `text-keys-by-bytes`, 2026-11-24

Stage 4 of the measurement plan of the OpenSpec change
`text-keys-by-bytes`: TessSort orders a string key of a collation that
orders by bytes, and `bytea`, by a word of its first eight bytes, and
its top-N sort reads the key in the bytes form and makes values only
for the rows that may stay (group 6 of its tasks). The stage is kept
when its first query, Q26, loses at least 15 per cent of instructions
against the commit the stage starts from, its other queries lose some,
and every other query counted, of ClickBench and of the heap, gains no
more than 1 per cent.

- **Tessera**, release builds: the start `9c76fd3e`, `main`, `start`
  in [runs.txt](runs.txt); the end `abbeeb22`, `end`, whose code is
  the branch's last commit's: the two differ by a scenario's name and
  the wrapping of the design.
- **PostgreSQL** 19rc2, release, without LLVM, with pg_pmu.
- **Data**: `hits.parquet`, 100 M rows, 14.8 GB, in the page cache.
- **Method**: `cargo clickbench run --mode on --counters --tries 5
  --workers 0`, one query a run, five readings after one that warms up,
  their medians; instructions decide, cycles are beside them. The
  heap's cases by `cargo ab measure --counters --base 9c76fd3e --family
  sort --family anykey`. pg_duckdb's cycles are those of stage 3's run,
  `duckdb`, of the same server and file, with one thread of DuckDB.
- **Q34 and Q35** spill 11 GB and depend on the page cache: counted
  with `--tries 1 --timeout 150`, the start and the end back to back.
- **Machine**: Apple M5 Pro, 18 cores, 64 GB.
- **Not counted**: Q23 and Q29, whose every reading passes the timeout
  of 60 s.

## Hypotheses

Written before any run:

1. Q26, the ten first search phrases, runs 25 to 40 per cent fewer
   instructions: its top-N sort makes each row's word from the bytes
   form instead of the core's abbreviated key, which hashes the string,
   and asks the scan for values only for the rows that may beat the
   worst kept.
2. Q27, ordered by the time and then the phrase, runs 5 to 20 per cent
   fewer: the phrase is its second key, read as bytes.
3. Every other query of ClickBench within 1 per cent.
4. Of the heap's `sort`, `order_text_c` and `topn_text_c` fewer; every
   other case of `sort` and `anykey` within 1 per cent.

## Result

Billions of instructions against the start; lower is better:

| query | what | start | end | change |
|---|---|--:|--:|--:|
| Q26 | the first phrases | 59.718 | 52.307 | -12.4 % |
| Q27 | by the time, then the phrase | 78.667 | 71.124 | -9.6 % |
| Q34 | URL, a pair | 345.314 | 341.790 | -1.0 % |
| Q35 | 1 and URL, a pair | 353.923 | 348.640 | -1.5 % |
| `order_text_c` | text under C, sorted whole | 7.658 | 4.587 | -40.1 % |
| `topn_text_c` | text under C, top-N | 1.347 | 1.004 | -25.5 % |

Q26's cycles fall from 17.5 to 15.3 billion, Q27's from 22.5 to 20.2.
Of the 36 other queries, every one is within 0.3 per cent but Q2,
-1.15, whose readings of one build alternate between two counts. Every
other case of `sort` and `anykey` is within 0.3 per cent
([ab-end.md](ab-end.md)). Q34 and Q35, one reading each, differ by up
to 1.5 per cent between two builds of one code back to back, `start34b`
and `end34b`, and by 3 to 5 per cent when the start and the end are
counted 25 minutes apart, `start34` and `end34`: only the pair back to
back compares, and it shows no gain.

So hypotheses 2, 3 and 4 hold, and the first does not: Q26 lost 12.4
per cent, not 15.

## Why Q26 lost less

A profile of Q26 at the end, 4 s of `sample` once a millisecond (2 443
samples, [profile-q26.txt](profile-q26.txt)): 94 per cent in the
Parquet reader and the system's calls under it, the decompression of
Snappy pages 34, the check of UTF-8 16, copies of memory 12; TessSort
and the kernels 4 per cent. What the stage removes, the making of each
row's value and its abbreviated key, was 12 per cent of the query; the
plan's 25 to 40 assumed a larger share. The reading of the column is
the reader's, which no consumer of the form can lighten.

Against pg_duckdb, by the medians of the cycles at one thread, Q26
takes 2.7 times DuckDB's 5.7 billion and Q27 2.6 times; over the 39
queries Tessera's end runs 2.06 times DuckDB's cycles by their
geometric mean ([counts.txt](counts.txt)).

The maintainer kept the stage with Q26 12.4 per cent below its start.

## Files

- [runs.txt](runs.txt): every run, by its side and query: `start`,
  `end`, the pairs `start34`, `end34`, `start34b`, `end34b`,
  `start34c` and `end34c`, and stage 3's `duckdb`.
- `source-start.txt` and `source-end.txt`: the sources of a run of
  each, its commit, server, settings, files and the hashes of the
  libraries built.
- [counts.txt](counts.txt): each side against its base by
  [compare.sh](compare.sh), the cycles by [cycles.sh](cycles.sh).
- [ab-end.md](ab-end.md): the report of `cargo ab measure` of the
  heap; [profile-q26.txt](profile-q26.txt): the functions of Q26's
  profile.

The runs' raw data stay in `target/bench-runs/` of the machine.
