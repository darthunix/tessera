# TessAgg's keys by bytes, stage 2 of `text-keys-by-bytes`, 2026-11-24

Stage 2 of the measurement plan of the OpenSpec change
`text-keys-by-bytes`: TessAgg's string keys read in the bytes form, the
array from a source dictionary's index to a key's number, and the
ungrouped `COUNT(DISTINCT)` of a string (group 4 of its tasks). The
stage is kept when its first query, Q13, loses at least 15 per cent of
instructions against the commit the stage starts from, its other
queries lose some, and every other query counted, of ClickBench and of
the heap, gains no more than 1 per cent.

- **Tessera**, release builds: the start `76d43940`, whose code is that
  of stage 1's end `94060934` (the two commits between them change a
  script of CI and a contract), so that stage 1's runs of its end stand
  for the start's, `start` in [runs.txt](runs.txt); the first count
  `ae769610`, 4.3 before the fixes below, `first`; the end `32a3046f`,
  `keys`, whose tree differs from the branch's `febe8234` by a comment,
  a test and the guide. The plan's second base, stage 4 of
  `parquet-fdw`, is `531b5d7d`, stage 1's `base`.
- **PostgreSQL** 19rc2, release, without LLVM, with pg_pmu.
- **Data**: `hits.parquet`, 100 M rows, 14.8 GB, in the page cache.
- **Method**: `cargo clickbench run --mode on --counters --tries 5
  --workers 0`, one query a run, five readings after one that warms up,
  their medians; instructions decide, cycles are beside them. The
  heap's cases by `cargo ab measure --counters --base 76d43940 --family
  sort --family anykey`. By the maintainer's word a count needs no idle
  machine.
- **Q34 and Q35** spill 11 GB, and pg_pmu counts the kernel's reads of
  it too, which depend on what the page cache holds: they were counted
  with `--tries 1 --timeout 150`, the start and the end back to back,
  `start34` and `keys34`, and only such a pair compares.
- **Machine**: Apple M5 Pro, 18 cores, 64 GB.
- **Not counted**: Q23 and Q29, whose every reading passes the timeout
  of 60 s.

## Result

Billions of instructions against the start; lower is better:

| query | what | start | end | change |
|---|---|--:|--:|--:|
| Q6 | distinct phrases | 136.779 | 74.789 | -45.3 % |
| Q13 | phrases counted, a filter | 76.995 | 71.660 | -6.9 % |
| Q15 | engine and phrase | 89.806 | 84.399 | -6.0 % |
| Q17 | user and phrase, ordered | 245.812 | 192.835 | -21.6 % |
| Q18 | user and phrase, a limit | 174.518 | 124.858 | -28.5 % |
| Q19 | user, minute and phrase | 478.729 | 428.200 | -10.6 % |
| Q34 | URL, a pair | 450.075 | 351.974 | -21.8 % |
| Q35 | 1 and URL, a pair | 459.343 | 360.014 | -21.6 % |
| Q37 | URL of a counter | 2.406 | 1.806 | -24.9 % |
| Q38 | title of a counter | 2.194 | 1.718 | -21.7 % |
| Q39 | URL of a counter, an offset | 0.638 | 0.638 | 0.0 % |
| Q40 | sources and URL | 4.584 | 3.824 | -16.6 % |

Of the 29 other queries, 21 are the same within 0.1 per cent and 8
more, the most Q3 and Q7 +0.2 per cent and Q2 +1.5. Q2 has states of
its own, whatever the build: of one build, its readings alternate
between two counts 38.5 M apart, or hold one. Counted again in turn,
the start and the end, `q2-start-1`, `q2-head-1`, `q2-start-2` and
`q2-head-2`, the end is -0.2 and +0.4 per cent of the start in each
pair. Against stage 4 of `parquet-fdw`, with stage 1 under it, Q2 is
+1.4 per cent, Q3 +1.1, Q1 +0.6, every other query within 0.4.

The heap: of `sort`, 23 cases within 0.2 per cent; of `anykey`, the
joins the same, `any_text_col`, a grouping by 450 000 values of text,
4.8 per cent fewer, and the groupings of small dictionaries 0.8 to
0.95 per cent more: the loop the lookups by `Datum` and by bytes share
asks each row's form and, of 4.2, whether its index is cached.

So the stage keeps every rule but its first: Q13 lost 6.9 per cent, not
15. Q39, whose filter leaves a few rows to group, loses nothing, as it
cannot. The maintainer kept the stage, for the reason the next section
gives.

## Why Q13 lost less

Q13 counts the rows of each search phrase that is not empty. A profile
of it at the end, 4 s of `sample` once a millisecond (2 881 samples,
`target/bench-runs/prof-q13-keys.txt` of the machine): 53 per cent in
the Parquet reader, its snappy decompression 24, the check of UTF-8
over each window 11, the source's test of the filter 3, the offsets of
the pages; 18 per cent in copies and comparisons of memory; the
grouping, its lookups, hashes and table, some 17. What the stage
removes, a palloc and a copy of every row's phrase, was 7 per cent of
the query: the plan's 20 to 35 assumed the making of `Datum` values a
larger share of it than the reading of the column, which no consumer
of the form can remove. `hash_bytes` alone takes 6 per cent: most rows
are still hashed by their bytes, not found by an index of 4.2's array.

## What the count found

The first count, `first`, broke the rule twice. Each fix was a
hypothesis written before it ran:

1. Q1, `count(*)`, +1.3 per cent, Q3 +0.7: 4.3's loop over the
   aggregates in every batch, asking whether an argument came as bytes.
   Run only where an argument may come so, which the node notes once:
   Q1 +0.2, Q3 +0.2. Kept, in 4.3.
2. `anykey`'s groupings of the heap, 0.5 to 1.0 per cent more,
   `any_text_col` +1.01: the shared loop's tests of a row's form. Each
   form a loop of its own up to the table's calls, `split`: Q6 and Q17,
   of bytes, 0.9 per cent more than the shared loop; not kept.
3. Past the caches the lookup by `Datum` hashed every row twice, in the
   prefetch and in the loop: kept from the prefetch, `prefetch`,
   `any_text_col` 4.8 per cent fewer than the start, the small
   dictionaries 0.2 to 0.3 more than before it, and Q13 and Q17 2 and 3
   per cent fewer, Q17's spilled rows coming back as `Datum` values.
   Kept, a commit of its own.

The first count's 4.1 also found the reader's bytes form walking every
row of its window on each ask, and a spill's rows checked and copied
twice: both fixed before, in their commits.

## Files

- [runs.txt](runs.txt): every run, by its side and query: `base`,
  `start`, `first`, `keys`, the pairs `start34-first`, `keys34-first`,
  `start34` and `keys34`, the tries `split` and `prefetch`, and Q2's
  pairs.
- `source-first.txt`, `source-keys.txt`, `source-start34.txt` and
  `source-keys34.txt`: the sources of a run of each, its commit, server,
  settings, files and the hashes of the libraries built.
- [counts.txt](counts.txt): each side against its base, by
  [report.sh](report.sh), and the reports of `cargo ab`.

The runs' raw data stay in `target/bench-runs/` of the machine.
