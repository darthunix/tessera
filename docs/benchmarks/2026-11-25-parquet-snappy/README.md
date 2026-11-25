# The reader's decoder of Snappy, `parquet-snappy`, 2026-11-25

The count of the OpenSpec change `parquet-snappy`: the Parquet reader
decompresses the Snappy pages itself, through the `parquet` crate's
lower entry, by a port to Rust of the loop of Google's snappy 1.2.1,
where the crate decompressed them by the crate `snap`. Cycles decide,
by the maintainer's choice for this change: the decoder does the same
work with far fewer branch misses, which instructions do not show. The
change is kept when Q28 loses at least 12 per cent of its cycles, Q26
at least 8, and no query counted gains more than 2.

- **Tessera**, release builds: the base `d16e43c8`, `main`; the end
  `5815c83e`, the change's last code.
- **PostgreSQL** 19rc2, release, without LLVM, with pg_pmu.
- **Data**: `hits.parquet`, 100 M rows, 14.8 GB, in the page cache.
- **Method**: `cargo clickbench run --mode on --counters --tries 5
  --workers 0`, one query a run, five readings after one that warms up,
  their medians. The base and the end in turns, twice: `base-h`,
  `end-h`, `base-i`, `end-i` in [runs.txt](runs.txt). pg_duckdb's
  cycles are those of the run `duckdb` of the sort by bytes, of the
  same server and file, with one thread of DuckDB.
- **Queries**: at the maintainer's word, a count of about 30 minutes:
  Q28, Q26, Q21, Q22, Q13 and Q25, which read `URL`, `Title`, `Referer`
  or `SearchPhrase`, and Q3 and Q8, of numbers only. The others were
  not counted.
- **Machine**: Apple M5 Pro, 18 cores, 64 GB, on mains power.

## Hypotheses

Written before the runs:

1. Q28: 15 to 22 per cent fewer cycles; Q26: 9 to 14.
2. Q21, Q22, Q13, Q25: fewer cycles, by the decoder's share of each.
3. Q3 and Q8: within 2 per cent of cycles.
4. The queries of numbers: their instructions may grow, by up to two
   fifths of the decoder's share.
5. The geometric mean over pg_duckdb's cycles: from 1.93 to below 1.8.

## Result

Billions of cycles at the base and at the end in each pair, the
change, and the end's cycles over pg_duckdb's; lower is better:

| query | what | base | end | change, two pairs | over pg_duckdb |
|---|---|--:|--:|--:|--:|
| Q28 | `avg(length(URL))` | 53.7 | 39.2 | -25.6, -27.0 % | 1.72 |
| Q26 | the first phrases | 13.5 | 12.1 | -11.3, -10.2 % | 2.14 |
| Q21 | `URL LIKE '%google%'` | 23.1 | 16.3 | -28.7, -29.3 % | 0.55 |
| Q22 | phrases with `google` | 57.7 | 37.7 | -34.6, -34.7 % | 2.09 |
| Q25 | phrases by the time | 9.7 | 8.5 | -12.9, -11.7 % | 1.09 |
| Q13 | grouped by the phrase | 22.0 | 20.0 | -7.6, -9.2 % | 2.36 |
| Q3 | sums of two numbers | 4.7 | 4.4 | +0.3, -7.7 % | 4.51 |
| Q8 | grouped by a number | 1.4 | 1.4 | -0.5, +1.8 % | 3.27 |

The bases and ends of the table are those of the second pair. The
instructions fell 3.2 to 9.7 per cent on the queries of strings and
stayed within 0.5 per cent on Q3 and Q8. Q3's base read 4.38 billion
cycles in the first pair and 4.72 in the second, a spread of 3.4 per
cent among its own readings: the -7.7 is that base's noise. Before the
change these queries ran in 2.33 (Q28), 2.39 (Q26), 0.78, 3.21, 1.22
and 2.60 times pg_duckdb's cycles.

So hypotheses 1 to 4 hold, Q28 past its range; the fifth was not
counted, as eight queries cannot give the geometric mean of all. The
change's thresholds hold, and no query gains.

## The decoders alone

[decoders.md](decoders.md) holds the counts of the decoder against
`snap`'s on 128 MB of compressed pages of each of eight columns, at
each commit that changed it, by `cargo clickbench decoders`, and
against DuckDB's snappy. At the end, per byte, over `snap`'s:

| column | instructions | cycles | misses a KB, ours and `snap`'s |
|---|--:|--:|--:|
| URL | 0.840 | 0.446 | 7.3, 46.1 |
| Title | 0.849 | 0.547 | 5.9, 34.9 |
| Referer | 0.821 | 0.472 | 8.1, 37.8 |
| SearchPhrase | 0.868 | 0.594 | 6.0, 43.5 |
| EventTime | 1.034 | 0.890 | 1.0, 2.8 |
| UserID | 0.988 | 0.557 | 1.7, 9.3 |
| WatchID | 1.004 | 1.004 | 0.0, 0.0 |

Against DuckDB's snappy, the object of the build pg_duckdb loads: 0.98
to 0.99 of its cycles on the strings, 1.00 on `EventTime`, 0.71 on
`UserID`, with 8 to 10 per cent fewer instructions.

The loop as first built took 1.10 to 1.15 of DuckDB's cycles with the
same instructions and misses. Two tries followed, each written down
before it ran: a prefetch of the input 128 bytes ahead each turn, as
Google's loop has, changed nothing and added 3 per cent of
instructions (dropped); the position in the input carried as a pointer,
not an index, which takes an addition off the chain from one tag's load
to the next, brought it to 0.98.

## The reader's own pages alone

The change's first step read the pages through the crate's lower entry
with `snap` still decoding, so that its cost was counted alone: Q2,
Q8, Q13, Q21, Q22, Q26, Q28, Q37, Q40 and Q41 against the base, `base`
and `end21` in [runs.txt](runs.txt), then Q2, Q8 and Q26 in turns
(`base-b` to `end-g`). The instructions fell or stayed; the cycles
stayed within 2 per cent but Q26's, 1.5 to 4.7 per cent more in every
pair. Neither the crate's way of allocating a page's output nor every
selection read by selectors brought it back; `snap`'s decoder, a third
of Q26's profile, lies elsewhere in that step's library. The maintainer
took the step with it; the end above covers it.

## Files

- [runs.txt](runs.txt): every run, by its side and query.
- [compare.sh](compare.sh): a side against its base, instructions and
  cycles; [duckdb.sh](duckdb.sh): a side's cycles over pg_duckdb's.
- `source-base.txt`, `source-end.txt` and `source-duckdb.txt`: the
  sources of a run of each, its commit, server, settings, files and the
  hashes of the libraries built.
- [decoders.md](decoders.md): the counts of the decoders.

The runs' raw data stay in `target/bench-runs/` of the machine.
