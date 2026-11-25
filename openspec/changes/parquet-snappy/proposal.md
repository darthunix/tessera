## Why

The Parquet reader spends more time decompressing Snappy pages than
anything else. The `parquet` crate decompresses each page through the
crate `snap`, a port of Snappy to Rust. Profiles taken with the macOS
tool `sample`, which records the running function once a millisecond,
show its decoder on top of the stacks:

- Q26, `ORDER BY SearchPhrase LIMIT 10`: 829 of 2 443 samples, 34 per
  cent;
- Q28, `avg(length(URL))` by `CounterID`: 1 051 of 2 854 samples, 37
  per cent.

Tessera runs ClickBench's queries in 1.93 times pg_duckdb's cycles at
one thread, by their geometric mean; Q26 in 2.40 times, Q28 in 2.33.

DuckDB decodes Snappy with Google's snappy 1.2.1. Its decoder and
`snap`'s were counted on the same compressed pages of `hits.parquet`,
on the M5 Pro, each into one reused buffer. On the columns of strings,
DuckDB's takes 45 to 61 per cent of `snap`'s cycles. It runs only 4 to
10 per cent fewer instructions. It mispredicts 6 to 8 branches a
kilobyte where `snap` mispredicts 35 to 46, and the cycles saved are 23
to 26 for each miss avoided on every one of these columns:

| column | cycles | instructions | misses a KB, `snap` | DuckDB |
|---|--:|--:|--:|--:|
| URL | 0.45 | 0.91 | 45.9 | 7.3 |
| Title | 0.55 | 0.92 | 35.0 | 5.7 |
| Referer | 0.48 | 0.90 | 37.7 | 8.0 |
| SearchPhrase | 0.61 | 0.96 | 43.6 | 5.9 |
| EventTime | 0.89 | 1.13 | 2.8 | 0.9 |
| UserID | 0.79 | 1.41 | 9.3 | 1.7 |
| WatchID | 1.00 | 1.01 | 0.02 | 0.02 |

The first two columns are DuckDB's counts over `snap`'s. Both decoders
gave the same bytes on every page. The gain is in the shape of the
loop: snappy 1.2 decodes the stream's elements without a branch on
each, through a table of 256 entries. With that loop, Q28 would lose
about a fifth of its cycles and Q26 about an eighth.

This change takes up the roadmap's entry `parquet-snappy`.

## What Changes

- A crate of the workspace, `crates/snap`, takes the place of `snap`
  in every build of the workspace, through Cargo's `[patch.crates-io]`.
  It is named `snap` and gives the four items and the error type the
  `parquet` crate calls. Its compressor, its error type and its reading
  of a stream's length are those of the real `snap`, on which it
  depends under another name. Only the decoder is its own. The
  `parquet` crate and Tessera's reader stay as they are.
- The decoder is a port to Rust of the decompression of a whole buffer
  in Google's snappy 1.2.1: the table, the loop without a branch on
  each element, the copies of short repeated patterns by a shuffle of
  bytes (NEON on AArch64, SSSE3 where the build targets it, plain code
  otherwise) and the careful steps at the end of a buffer.
- The decoder gives the same bytes as `snap` on every valid stream, and
  an error on every stream `snap` refuses. No stream, valid or not,
  makes it read or write outside its buffers, or panic. Tests compare
  it with `snap`'s decoder; Kani proves the bounds of its steps; Miri
  runs its tests; a fuzz target compares the two decoders on any bytes;
  mutants judge its lines.
- CI fails when the patch is not used, so that the build never falls
  back to the real `snap`'s decoder unnoticed.
- `cargo clickbench` gains a mode that decodes the pages of a file's
  columns by both decoders and counts their instructions, cycles,
  branches and branch misses.
- Results, errors, EXPLAIN and the settings stay as they are. A damaged
  page still fails a query with `XX001`.
- Not in this change: a choice of the SSSE3 shuffle while running, on
  x86-64 builds that do not target it; a count on x86-64; Tessera's own
  decompression of pages; other codecs.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

None. What SQL sees does not change: `skip_specs` is set. The design
of `parquet-fdw` gains the decoder, and its contract a rule on the
decoder's memory.

## Impact

- Crates: a new `crates/snap`, the workspace's `Cargo.toml` and
  `Cargo.lock`. `tessera-parquet` does not change. The workspace takes
  no crate from outside that it does not already build.
- Tools: `tools/tessera-clickbench`, its mode of decoders.
- CI: the crate's proofs in the job of Kani, its tests in Miri's, its
  fuzz target in `fuzz.yml`, the check of an unused patch.
- Public headers, settings, SQL suites: none.
- Contracts: `parquet-fdw`, a rule on the decoder's memory and its
  forecast of another decompression of pages.
- Licenses: the port carries the notice of Google's snappy, BSD-3.
- Roadmap: the entry `parquet-snappy` leaves it at the close.
