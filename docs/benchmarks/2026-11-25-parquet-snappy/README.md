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
  `09a6d50a`, the change's last code with the reviews' fixes, and
  `5815c83e`, the code before them.
- **PostgreSQL** 19rc2, release, without LLVM, with pg_pmu.
- **Data**: `hits.parquet`, 100 M rows, 14.8 GB, in the page cache.
- **Method**: `cargo clickbench run --mode on --counters --tries 5
  --workers 0`, one query a run, five readings after one that warms up,
  their medians. The base and the end in turns, twice: `base-j`,
  `end-j`, `base-k`, `end-k` in [runs.txt](runs.txt) for the end, and
  `base-h` to `end-i` for the code before the reviews' fixes. pg_duckdb's
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

Billions of cycles at the base and at the end in the second pair, the
change in each pair, and the end's cycles over pg_duckdb's; lower is
better:

| query | what | base | end | change, two pairs | over pg_duckdb |
|---|---|--:|--:|--:|--:|
| Q28 | `avg(length(URL))` | 54.1 | 40.4 | -24.8, -25.4 % | 1.77 |
| Q26 | the first phrases | 13.8 | 11.8 | -12.4, -14.9 % | 2.07 |
| Q21 | `URL LIKE '%google%'` | 23.3 | 16.6 | -27.9, -28.7 % | 0.56 |
| Q22 | phrases with `google` | 58.2 | 38.3 | -27.7, -34.2 % | 2.12 |
| Q25 | phrases by the time | 9.5 | 8.5 | -10.3, -10.0 % | 1.09 |
| Q13 | grouped by the phrase | 22.1 | 21.1 | -5.1, -4.3 % | 2.50 |
| Q3 | sums of two numbers | 4.4 | 4.3 | -2.3, -1.7 % | 4.48 |
| Q8 | grouped by a number | 1.44 | 1.48 | +10.5, +2.4 % | 3.35 |

The first pair's end ran noisy: the spread of its readings reached 17
per cent on Q13 and 8 on Q22, against 1 or less elsewhere. The
instructions fell 1.3 to 9.4 per cent on Q13, Q21, Q22, Q26 and Q28,
rose 0.7 on Q25, stayed on Q3, and rose 3.0 on Q8 in both pairs.
Before the change these queries ran in 2.33 (Q28), 2.39 (Q26), 0.78,
3.21, 1.22 and 2.60 times pg_duckdb's cycles.

So hypotheses 1 to 3 hold, Q28 past its range; the fourth holds but on
Q8, whose instructions rose 3 per cent; the fifth was not counted, as
eight queries cannot give the geometric mean of all. The thresholds for
Q28 and Q26 hold. Q8's cycles rose past the threshold of 2 per cent in
both pairs; counted again in two more pairs (`base-l` to `end-m`), +0.6
and -7.2 per cent, the last over a base whose readings spread 15 per
cent. The maintainer took the change with Q8 so.

**Q8's instructions.** The code before the reviews' fixes ran Q8 in 0.1
per cent more instructions than the base. A count in turns of three
variants of the end (`end-l` to `noskip-m`) splits the 3 per cent:

| variant | Q8 | Q13 | Q25 |
|---|--:|--:|--:|
| the end | +3.0 % | -1.4 % | +0.7 % |
| no header's bytes kept, its size unchecked | +2.2 % | -1.4 % | +0.6 % |
| no trailing skip, the crate's default policy | +2.6 % | -2.4 % | -4.7 % |

Keeping a page's header costs Q8 0.8 per cent of its instructions; the
trailing skip, which makes the crate read by selectors, 0.4 on Q8 and 5
on Q25, with no more cycles. About 2 per cent of Q8's are in both
variants and not in the code before the fixes, and not explained: Q8's
instructions repeat within 0.1 per cent, and pg_pmu counts the kernel
on macOS, so the work of the system for the pages' memory is a
candidate, not shown.

**Before the reviews' fixes**, which put the selectors back and check a
page's size against its header, the same count gave Q28 -25.6 and -27.0
per cent of cycles, Q26 -11.3 and -10.2, Q21 -28.7 and -29.3, Q22 -34.6
and -34.7, Q25 -12.9 and -11.7, Q13 -7.6 and -9.2, Q3 +0.3 and -7.7,
Q8 -0.5 and +1.8, with Q8's instructions within 0.2 per cent.

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

- [runs.txt](runs.txt): every run, by its side and query; `nocap` and
  `noskip` are the variants of the end above.
- [compare.sh](compare.sh): a side against its base, instructions and
  cycles; [duckdb.sh](duckdb.sh): a side's cycles over pg_duckdb's.
- `source-base.txt`, `source-end.txt` and `source-duckdb.txt`: the
  sources of a run of each, its commit, server, settings, files and the
  hashes of the libraries built.
- [decoders.md](decoders.md): the counts of the decoders.

The runs' raw data stay in `target/bench-runs/` of the machine.
