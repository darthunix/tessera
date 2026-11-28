## Context

See proposal.md, "Why", for the numbers. What the code does today:

- `Like::parse` (`crates/tessera-kernels/src/text.rs`) cuts a pattern
  of literals and `%` signs into at most 32 pieces, notes whether the
  first is anchored at the start and the last at the end, and refuses a
  pattern with `_` or an escape. `Like` is `Copy`: its pieces are an
  array of offsets into the pattern.
- `Like::matches` checks the anchored pieces by `starts_with` and
  `ends_with`, then finds each inner piece in what is left, in order,
  by `find`: `memchr` of the piece's first byte, then a comparison of
  the rest, then `memchr` from the next byte.
- `tess_text_like` parses the pattern once a call, a batch of up to 64
  rows, and runs the match for each row asked. The rows it does not
  take, a pattern the matcher refuses or a multibyte encoding other
  than UTF-8, go to the core's function in `kernels/text.c`.
- The Parquet source parses the pattern of a clause once a window of
  1 024 rows, or once a dictionary, and runs the same match on the
  stored values (`crates/tessera-parquet/src/clause.rs`).
- `memchr` 2.8 is a dependency of the kernels. Its `memmem::Finder` is a
  searcher made once for a needle: on AArch64 and x86-64 it looks for a
  pair of the needle's rare bytes by vector instructions and checks the
  whole needle where both are found; under 64 bytes of string its
  `memmem::find`, made anew each call, takes a Rabin-Karp search.

## Goals / Non-Goals

Goals:

- A pattern's inner pieces found in fewer cycles a row, on strings of
  TPC-H's and ClickBench's lengths, whatever the piece's first byte.
- The cost of making the searchers, once a call of up to 64 rows, does
  not show on short strings or on a match near the start.
- No result changes; the patterns refused stay refused.

Non-goals: the anchored pieces and the exact pattern, which compare at
one place already; `_` and escapes; patterns of `ILIKE` and of
nondeterministic collations, which the kernels do not take; a cache of
a parsed pattern from one call to the next, which would change the
kernels' entry points.

## Decisions

### Which searcher

Three ways were counted on the family's cases by cycles, against the
commit that adds the family:

1. **`memmem::Finder` for each of the first four inner pieces, made in
   `Like::parse`,** found the pieces in the fewest cycles: TPC-H query
   13's predicate at 0.41 of the base, `'%google%'` over URLs 0.63,
   short strings 0.75, a piece of one byte 0.93. Kept in `Like` itself,
   the four searchers took the guards that search nothing 3 to 6 per
   cent more cycles, as every call made, moved and dropped them.
2. **`memmem::find` a row** lost on most cases: a searcher made for each
   string past 64 bytes, `'%google%'` at 1.28 and a piece of one byte at
   1.68.
3. **`memchr` of the piece's rarest byte**, the byte that `memchr`'s
   public `arch::all::packedpair::Pair::new` ranks first, gained on the
   predicate alone, 0.72, and not on `'%google%'`, whose rarest byte is
   about as common as its first.

Kept: the first, with its searchers boxed and made only for a pattern
that has an inner piece, so that a pattern that searches nothing costs
no more than before: the guards at 0.99 to 1.01 of the base's cycles,
and the predicate at 0.42, `'%google%'` 0.66, the short strings 0.75.
A searcher made once a call costs some 3 500 instructions a batch, so
the short strings run 12 per cent more instructions for 25 per cent
fewer cycles. A piece past the fourth is found by `memchr` of its first
byte, as before.

Refused:

- **A search of our own over vectors.** `memchr` is used by hundreds of
  crates and holds the vector searches for both machines; a copy would
  be ours to keep on each.
- **A crate of its own for substrings,** such as `aho-corasick`, which
  builds a machine for many needles: one piece at a time is what a
  `LIKE` needs, and `memchr` is already here.

### Where the searchers live

In `Like`, beside its pieces, so that both callers, the batch function
and the Parquet source's clauses, take them without a change of their
code. The pattern stays borrowed: a `Finder` made from a borrowed
needle keeps its lifetime, as `Like` keeps the pattern's.

## Was and will be

- **The search of an inner piece.** Was: `memchr` of its first byte and
  a comparison at each place found. Will be: a searcher of `memmem`
  made once a pattern for the first four, as before for the others.
- **`Like`.** Was: offsets of its pieces, `Copy`. Will be: the same, and
  a box of the searchers of its first four inner pieces, made only for
  a pattern that has one; `Clone` alone.

## What we do not do

- **A parsed pattern kept across calls.** A call is a batch of up to 64
  rows; keeping the pattern for the next would need a handle in the
  kernels' entry points, a change of the C API. Taken only if the
  measurement shows the making of the searchers dear.
- **`_` and escapes.** The rows of such a pattern go to the core's
  function, as today.
- **The anchored pieces.** They compare at one place.

## Risks / Trade-offs

- [A searcher made a call costs more than it saves on short strings] →
  a guard of strings of 8 to 16 bytes, and the third way, which makes
  nothing.
- [A wrong match] → the property test of the matcher against a
  reference by dynamic programming gets strings past 64 bytes, pieces
  at every place and pieces of one byte to many, so that the vector
  loops run; the suites compare `LIKE` with the core's.
- [A way fast on one machine and slow on the other] → the counts are of
  the M5 Pro; the x86-64 path of `memchr` is its own, which the
  measurement does not see. Linux x86-64 is the target; the report says
  so, and the counts on x86 follow when `bench-in-four-cells` gives
  them.

## Measurement plan

On the M5 Pro, PostgreSQL 19 with `pg_pmu`, cycles deciding,
instructions beside, every reading kept. The base is the commit that
adds the family, which changes no code.

- `bench/pg`, a new family `like` over a table of its own of 2 million
  rows: a column of comments of English words, 10 to 80 bytes, as
  TPC-H's `o_comment`; a column of URLs of 40 to 120 bytes, as
  ClickBench's; a column of strings of 8 to 16 bytes. `cargo ab measure
  --counters --cycles --rounds 2 --deadline 300`, then without
  `--cycles` for the instructions. Cases, each a count of the rows that
  match:
  - two inner pieces of common first bytes over the comments,
    `NOT LIKE '%special%requests%'`, the predicate of TPC-H's query 13;
  - one inner piece over the URLs, `LIKE '%google%'`, rare;
  - one inner piece found near the start of most strings;
  - one inner piece of one byte, `'%e%'`;
  - one inner piece over the short strings, the guard of the cost of
    making the searchers;
  - guards that do not search: a prefix `'b-1%'`, an exact pattern, a
    suffix `'%.com'`, and a pattern with `_`, which the core matches.
- ClickBench over `hits.parquet`, `cargo clickbench run --mode on
  --counters --tries 5 --workers 0`, one query a run, the base and the
  end in turns, twice: Q21 and Q22, which filter `URL LIKE '%google%'`
  on the Parquet source's stored values and in the kernels; Q1 with
  Tessera off the control.
- TPC-H's query 13 at scale factor 1, its time by `cargo tpch`, serial,
  the base and the end in turns, after the counts.
- Before the counts, the machine code of the match's loop at the base
  and at each way, by `tessera-bench --disasm`.

Before the runs: the agent asks for an idle machine and records the
power source; release builds are installed and checked; plans are
compared.

Hypotheses, written before the runs:

1. The case of TPC-H's predicate: 30 to 60 per cent fewer cycles.
2. The URLs' `'%google%'`: 20 to 50 per cent fewer.
3. The short strings, the match near the start and the piece of one
   byte: within 2 per cent, or fewer.
4. The guards that do not search: within 2 per cent.
5. ClickBench's Q21 and Q22: fewer cycles, Q21 at least 10 per cent;
   the control within 2.
6. TPC-H's query 13: at least 20 per cent less time.

How a case is judged: a side's spread is the difference of its two
turns; a change smaller than the larger spread of the two sides, or
than the control's move, is no change; a side past 2 per cent is
counted once more. The change is kept when hypotheses 1, 3 and 4 hold
and no case takes more than 2 per cent more cycles than its base. A
confirmed excess is profiled and searched as CONTRIBUTING.md says
before the maintainer is asked. The report is kept under
`docs/benchmarks/`.

## Decisions of the maintainer

- 2026-11-28: the entry `like-substring-search` is taken after the
  close of `text-keys-by-bytes`; the proposal, the design and the tasks
  approved.

## Files

- `crates/tessera-kernels/src/text.rs`: the search of an inner piece in
  `Like`.
- `crates/tessera-kernels/tests/text.rs`: the property test of the
  matcher.
- `bench/pg/like.sql`, `bench/pg/README.md`: the family.

## Tests

- `crates/tessera-kernels/tests/text.rs`: the matcher against a
  reference by dynamic programming over strings of 0 to 300 bytes,
  pieces of 1 to 40 bytes at every place, repeated bytes, and pieces
  found, found late and not found.
- The SQL suites that compare `LIKE` with the core's, over heap tables
  and the Parquet source.
- CI's mutants over the changed lines.
