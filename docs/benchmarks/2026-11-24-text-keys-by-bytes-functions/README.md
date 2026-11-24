# The string functions by bytes, stage 3 of `text-keys-by-bytes`, 2026-11-24

Stage 3 of the measurement plan of the OpenSpec change
`text-keys-by-bytes`: the text kernels, `length`, `LIKE`, `=`, `substr`
and the others, read a string column in the bytes form, and the
expression compiler gives it to them where the batch offers it (group
5 of its tasks). The stage is kept when its first query, Q28, loses at
least 15 per cent of instructions against the commit the stage starts
from, and every other query counted, of ClickBench and of the heap,
gains no more than 1 per cent.

- **Tessera**, release builds: the start `f8ee89ef`, `main`, `start`
  in [runs.txt](runs.txt); the end `81f23392`, `end`, whose tree
  differs from the branch's `1f61864e` by the inlining of try 3 below,
  which changes the machine code of `tess_text_like` and
  `tess_text_pieces` alone, functions no plan of the queries counted
  runs: every `LIKE` of their plans is the source's own check
  (`Source Exact`).
- **PostgreSQL** 19rc2, release, without LLVM, with pg_pmu; one
  version, by the maintainer's word.
- **Data**: `hits.parquet`, 100 M rows, 14.8 GB, in the page cache.
- **Method**: `cargo clickbench run --mode on --counters --tries 5
  --workers 0`, one query a run, five readings after one that warms up,
  their medians; instructions decide, cycles are beside them. pg_duckdb
  1.2.0 the same way, `--mode duckdb`, one thread of DuckDB. The heap's
  cases by `cargo ab measure --counters --base f8ee89ef --family sort
  --family anykey --family rowwise`. A count needs no idle machine.
- **Q34 and Q35** spill 11 GB and depend on the page cache: counted
  with `--tries 1 --timeout 150`, the start and the end back to back,
  `start34` and `end34`.
- **Machine**: Apple M5 Pro, 18 cores, 64 GB.
- **Not counted**: Q23 and Q29, whose every reading passes the timeout
  of 60 s.

## Hypotheses

Written before any run:

1. Q28, the average length of the URL by counter, `URL <> ''` checked
   by the source, runs 20 to 30 per cent fewer instructions: the scan
   makes no value of the URL, a copy behind a header for each row the
   filter keeps, and the length kernel reads the same bytes in place.
2. Every other query of ClickBench within 1 per cent of the start: no
   other counted query computes a text function in a batch over the
   Parquet table.
3. The heap's cases of `sort`, `anykey` (`substr` in keys) and
   `rowwise` (`LIKE` in a filter) within 1 per cent: the heap gives no
   bytes form, and the kernels' entry points test the argument's form
   once a call, not a row.

## Result

Billions of instructions against the start; lower is better:

| query | what | start | end | change |
|---|---|--:|--:|--:|
| Q28 | average length of the URL by counter | 261.983 | 188.623 | -28.0 % |
| Q34 | URL, a pair | 348.727 | 340.696 | -2.3 % |
| Q35 | 1 and URL, a pair | 359.035 | 342.527 | -4.6 % |

Q28's cycles fall from 78.4 to 58.6 billion. Of the 36 other queries,
every one is within 0.4 per cent but Q41, +1.1: a query of 0.6
billion, whose readings spread over 2 per cent within one run.
Counted again in turn, eleven readings each, `start41`, `end41`,
`start41b` and `end41b`, the end is -0.2 and 0.0 per cent of the
start. Q34 and Q35, one reading each, show no gain; the stage has no
reason to lower them, and a pair's difference of a few per cent is
the page cache's.

The heap ([ab-1-end.md](ab-1-end.md) at the end, before try 3; the
end's figures below are [ab-3-inlined.md](ab-3-inlined.md)): of
`sort`, 23 cases within 0.2 per cent; of `anykey`, every case within
0.6 per cent, the four of `substr` 0.5 to 0.6 more; of `rowwise`,
`row_like` and `row_like_count` 1.1 per cent more, the rest within
0.3. So hypotheses 1 and 2 hold, and 3 holds but for the two cases of
`LIKE` over the heap, which pass the rule by 0.1 per cent.

Against pg_duckdb, by the medians of the cycles at one thread
([counts.txt](counts.txt)), Tessera's end runs 2.08 times its cycles
over the 39 queries, by their geometric mean, and 1.86 times without
Q30, the sum of 90 columns `ResolutionWidth + k`, which DuckDB folds
into one sum (130 times). Q28 takes 2.6 times DuckDB's 22.8 billion;
Tessera runs fewer in Q4, Q20, Q21, Q39 and Q41 to Q43. Cycles move
with the machine's load, which the counts did not hold still: they
are a rough comparison.

## The way there

Each try was a hypothesis written before it ran:

1. A first count at `4ab9003c`, before the reviews' fixes, stopped at
   Q28, `first`, and the whole count at `4b0cf7fc`, `functions`:
   Q28 -28.0 per cent, Q3 +1.07 and +1.47, Q10 +0.69 and +1.23, no
   query of strings. TessAgg's read of its columns in their order
   asked, at each batch and for each column, whether a computed
   expression reads it as bytes, walking every chain; the reviews'
   fix made the walk longer.
2. That answer found once, at the first batch, `try2` then `end`: Q3
   +0.18, Q10 +0.09 per cent. Kept, in 5.3.
3. The heap at the end, [ab-1-end.md](ab-1-end.md): `row_like` +5.9,
   `row_like_count` +5.7 and `anykey`'s four cases of `substr` +3.9 to
   +4.1 per cent. With a loop for each form, the compiler stopped
   inlining the match of a pattern, its search of a piece and the
   bounds of a piece, a call a row
   ([1-kernels-end.txt](disasm/1-kernels-end.txt)). Inlined by force
   ([2-kernels-inlined.txt](disasm/2-kernels-inlined.txt)), the loops
   are the base's: `substr` +0.5 to +0.6, `LIKE` +1.1
   ([ab-3-inlined.md](ab-3-inlined.md)). Kept, in 5.1.
4. What is left is some 51 instructions a call of `LIKE`, 57 of
   `substr`, one call a batch of 64 rows: the checks of the argument's
   form on the call's way. Asking a batch for the form only where its
   operations have it, so that a heap batch's structure is not filled,
   changed nothing ([ab-4-ops-first.md](ab-4-ops-first.md)); reverted.

The maintainer kept the stage with the two cases of `LIKE` over the
heap 1.1 per cent above the start: the rest is the cost of a call that
takes either form, and no query of ClickBench runs that path, whose
`LIKE` the source checks.

## Files

- [runs.txt](runs.txt): every run, by its side and query: `start`,
  `first`, `functions`, `try2`, `end`, the pairs `start34` and
  `end34`, Q41's `start41`, `end41`, `start41b` and `end41b`, and
  `duckdb`.
- `source-start.txt`, `source-end.txt` and `source-duckdb.txt`: the
  sources of a run of each, its commit, server, settings, files and
  the hashes of the libraries built.
- [counts.txt](counts.txt): each side against its base by
  [compare.sh](compare.sh), and the cycles by [cycles.sh](cycles.sh).
- `ab-*.md`: the reports of `cargo ab measure` of the heap, numbered as
  the tries above; `disasm/`: the reports of `tessera-bench --disasm
  --module kernels` against the start.

The runs' raw data stay in `target/bench-runs/` of the machine.
