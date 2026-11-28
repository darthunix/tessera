# LIKE's pieces by memmem, `like-substring-search`, 2026-11-28

The count of the OpenSpec change `like-substring-search`. The kernels
match a pattern of `LIKE` made of literals and `%` signs by its pieces
between the signs; a piece neither at the start nor at the end of the
string, an **inner piece**, is searched for in it. The search was
`memchr` of the piece's first byte and a comparison of the rest at each
place found. It is now a searcher of `memchr::memmem` made once a
pattern for each of its first four inner pieces, which looks for a pair
of the piece's rare bytes by vector instructions; the searchers are
boxed and made only for a pattern that has an inner piece. Cycles
decide, by the maintainer's rule; instructions stand beside them, and
times stand for TPC-H's query 13.

- **Tessera**, release builds: the base `cfcbfecf`, task 2.1, which
  adds the family `like` and changes no code; the end `18f5e5cb`, task
  3.2. `cargo ab` builds both sides in its own directories, and takes
  the family from the branch for both; ClickBench's and TPC-H's base is
  a worktree at `cfcbfecf`, their end the branch's worktree, both built
  clean for the server below.
- **PostgreSQL** 19rc2, release, without LLVM, with pg_pmu, by the
  hashes in the sources ([source-base.txt](source-base.txt),
  [source-end.txt](source-end.txt)).
- **`like`**: the family's own table of 2 million rows, `cargo ab
  measure --base cfcbfecf --family like --counters --cycles --rounds 2
  --deadline 300`, then `--counters` alone for the instructions.
- **ClickBench**: `hits.parquet`, 100 M rows, in the page cache, `cargo
  clickbench run --mode <on|off> --counters --tries 5 --workers 0
  --queries <n> --files hits.parquet`, one query a run, the medians of
  the four readings after one that warms up; base and end in turns,
  query by query, two pairs ([runs.txt](runs.txt)).
- **TPC-H**: scale factor 1, `cargo tpch run --queries 13 --pairs 11`,
  serial, each tree's own cluster, base and end in turns, twice.
- **Machine**: Apple M5 Pro, 18 cores, 64 GB, idle at the maintainer's
  word, on mains, under `caffeinate -i`; a check of a quiet minute
  before each run left aside another agent's test under Miri, which
  took one core of its own, as the maintainer ruled.

## The ways tried

The design named three ways; each was counted on a branch that is never
merged, against `7d64c988`, which holds the same code as the base
([ways.md](ways.md)). By the median cycles over the base; lower is
better:

| case | way 1 | way 2 | way 3 | way 1b |
|---|--:|--:|--:|--:|
| `like_two_pieces` | 0.42 | 0.76 | 0.74 | 0.42 |
| `like_url_google` | 0.63 | 1.28 | 1.05 | 0.66 |
| `like_short` | 0.75 | 0.88 | 1.05 | 0.75 |
| `like_one_byte` | 0.93 | 1.67 | 1.12 | 0.93 |
| `like_near_start` | 0.99 | 1.30 | 1.12 | 0.99 |
| `guard_exact` | 1.06 | 1.02 | 1.03 | 1.01 |
| `guard_suffix` | 1.03 | 1.01 | 1.07 | 1.01 |

Way 1 makes a `memmem::Finder` for each of the first four inner pieces
in `Like::parse`; way 2 calls `memmem::find` a row, a searcher made for
each string past 64 bytes; way 3 calls `memchr` on the piece's rarest
byte by `memchr`'s ranking. Way 1 searched in the fewest cycles but took
the guards, which search nothing, 3 to 6 per cent more: its searchers,
kept in `Like` itself, were made, moved and dropped by every call.
Written before the try: boxed and made only for a pattern with an inner
piece, they cost the guards nothing. Way 1b held it and was kept. Way
3's run of cycles was itself unsettled: its control moved 5 to 15 per
cent.

## Hypotheses

Written in the design before the runs:

1. The case of TPC-H's predicate: 30 to 60 per cent fewer cycles.
2. The URLs' `'%google%'`: 20 to 50 per cent fewer.
3. The short strings, the match near the start and the piece of one
   byte: within 2 per cent, or fewer.
4. The guards that do not search: within 2 per cent.
5. ClickBench's Q21 and Q22: fewer cycles, Q21 at least 10 per cent;
   the control within 2.
6. TPC-H's query 13: at least 20 per cent less time.

The change is kept when hypotheses 1, 3 and 4 hold and no case takes
more than 2 per cent more cycles than its base.

## Result: the family

`end / base` by the minimum and the median of two rounds; the control
is the same with Tessera off ([family.md](family.md)); lower is better:

| case | what it is | cycles | control | instructions |
|---|---|--:|--:|--:|
| `like_two_pieces` | TPC-H 13's predicate | 0.421 / 0.436 | 1.000 | 0.746 |
| `like_url_google` | `'%google%'`, URLs | 0.654 / 0.662 | 1.008 | 1.027 |
| `like_short` | `'%ab%'`, 8 to 16 bytes | 0.748 / 0.727 | 1.004 | 1.117 |
| `like_one_byte` | `'%e%'` | 0.924 / 0.928 | 1.005 | 0.956 |
| `like_near_start` | `'%://%'` | 0.996 / 0.935 | 0.960 | 1.028 |
| `guard_exact` | no `%` | 0.977 / 0.987 | 1.003 | 1.001 |
| `guard_prefix` | `'https://www.g%'` | 1.019 / 0.995 | 0.954 | 1.003 |
| `guard_suffix` | `'%.html'` | 0.989 / 0.992 | 0.997 | 1.000 |
| `guard_underscore` | `'%sp_cial%'`, core's | 0.985 / 0.993 | 0.990 | 1.000 |

`guard_prefix`'s minimum, 1.9 per cent over the base, is within the
control's move of 4.6 and its median below the base: no change by the
design's rule of judging. Three cases take more instructions for fewer
cycles: the short strings 12 per cent, the making of a searcher once a
call, some 3 500 instructions a batch of 64 rows; `'%google%'` and the
piece near the start about 3, the searcher's setup over each string.

## Result: ClickBench

Q21 counts `URL LIKE '%google%'`, Q22 groups the phrases of those rows;
Q1 with Tessera off is the control. Billions of cycles at the base and
at the end, the change of cycles and of instructions; lower is better:

| query | pair | base | end | cycles | instructions |
|---|---|--:|--:|--:|--:|
| Q21 | first | 16.14 | 11.33 | -29.8 % | -0.10 % |
| Q21 | second | 16.13 | 11.37 | -29.5 % | -0.11 % |
| Q22 | first | 36.09 | 31.40 | -13.0 % | +0.32 % |
| Q22 | second | 36.08 | 31.45 | -12.8 % | +0.32 % |
| Q1, off | first | 24.07 | 25.50 | +6.0 % | +0.00 % |
| Q1, off | second | 23.96 | 23.99 | +0.1 % | -0.00 % |

The control's first pair ran in the processor's unstable state, the
end's readings 24.8 to 25.9 billion; its second pair is the control.
Q21 took 3.58 s at the base and 2.55 s at the end. Its instructions do
not move while its cycles fall by 30 per cent; the query was not
profiled to say which of its parts gave them.

## Result: TPC-H

Query 13 at scale factor 1, milliseconds, the medians of eleven pairs;
the core's time is the control:

| run | Tessera on, base | Tessera on, end | core, base | core, end |
|---|--:|--:|--:|--:|
| first | 126.0 | 86.9 | 292.5 | 290.0 |
| second | 125.6 | 87.1 | 288.5 | 305.4 |

The query takes 0.69 of its time at the base, 0.30 of the core's where
it took 0.43.

## Verdict

Every hypothesis holds: TPC-H's predicate at 0.42 of the base's cycles,
`'%google%'` at 0.65, the short strings at 0.75, the guards within 2 per
cent, ClickBench's Q21 29.5 to 29.8 per cent fewer cycles and Q22 13,
and TPC-H's query 13 31 per cent less time. No case takes more than 2
per cent more cycles than its base. The change is kept.

The counts are of the M5 Pro alone. `memchr`'s searchers take other
vector instructions on x86-64, which these runs do not see; Linux
x86-64 is the target platform.
