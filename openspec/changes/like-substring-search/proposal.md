## Why

The kernels match a pattern of `LIKE` made of literals and `%` signs,
such as `'%special%requests%'`, by its pieces between the signs. Each
piece not anchored at the start or the end of the string is found by a
search of the string: `memchr` of the piece's first byte, then a
comparison of the rest at each place found, then `memchr` again from
the next byte. Where the first byte is common in the strings, the
search stops at most of their bytes. TPC-H's `o_comment` is made of
English words, and the first byte of `special` is `s`.

TPC-H's query 13 counts the orders of each customer whose comment is
`NOT LIKE '%special%requests%'`, over the 1.5 million orders of scale
factor 1. With Tessera on and serial it took 137 ms against the core's
312, and a profile of it (macOS `sample`, one sample a millisecond) put
45 per cent of its time in `tess_text_like`, about 60 ms: some 40 ns a
row, a string of about 49 bytes.

The same matcher serves the Parquet source, which checks a clause of
`LIKE` on a file's stored values with it. ClickBench's queries 21 and
22 filter `URL LIKE '%google%'`, over 100 million URLs: Q21 runs 52.6
billion instructions, Q22 135.0, 3.23 times pg_duckdb's cycles.

The crate `memchr`, which the kernels already take, holds a search of a
substring, `memmem`: a searcher made once for a piece, which looks for
a pair of the piece's rare bytes, many bytes of the string a vector
instruction, and checks the whole piece only where both are found. It
takes up the roadmap's entry `like-substring-search`. It is work on
speed, so it comes as a change with a measurement plan.

## What Changes

- `Like`, the parsed pattern of the kernels, finds each piece between
  `%` signs by a searcher made once a pattern, in place of `memchr` of
  the first byte and a comparison. The anchored first and last pieces,
  a pattern without `%`, and the patterns the matcher refuses, with `_`,
  an escape or more than 32 pieces, stay as they are. Which searcher is
  kept is decided by the counts of the design's measurement plan, among
  `memmem`'s searcher made once a call, `memmem`'s search of a string
  without one, and `memchr` of the piece's rarest byte.
- The family `like` of `bench/pg` counts patterns of one and two pieces
  over strings like TPC-H's comments and ClickBench's URLs, frequent
  and rare first bytes, matches near the start, the end and none, and
  the patterns that stay as they are, as guards.
- No result changes: every match is the one the core's `LIKE` gives,
  which the existing tests compare and a property test of the matcher
  checks against a reference.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

None: the kernels' entry points keep their calls and their results,
and the Parquet source's clauses theirs. The change sets `skip_specs`.

## Impact

- `crates/tessera-kernels/src/text.rs`: the search of a piece in
  `Like`, and its tests; `crates/tessera-kernels/tests/text.rs`.
- The callers of `Like`, which keep their code: `tess_text_like` in
  `crates/tessera-capi/src/c/text.rs`, and the clauses of the Parquet
  source in `crates/tessera-parquet/src/clause.rs`.
- No public header, setting, format or SQL suite's result changes.
- `bench/pg/like.sql`, a new family, and `bench/pg/README.md`.
- No new dependency: `memchr` is already one of the kernels'.
