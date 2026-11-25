# Design

## Context

The `parquet` crate chooses a page's decompression itself. Its page
reader makes a codec from the column's compression when it is built,
and the builder of the Arrow reader that Tessera uses has no way to
give it another. For Snappy the codec is twenty lines around the crate
`snap`. It knows each page's decompressed size from the page's header,
sizes its output to it, filled with zeros, and calls
`snap::raw::Decoder::decompress`. Of `snap` it uses five items:
`raw::Decoder`, `raw::Encoder`, `raw::decompress_len`,
`raw::max_compress_len` and the type `Error`, whose variants are
public. The compressor serves the crate's writer, which the tests of
`tessera-parquet` use to make their files.

Cargo lets a workspace put a crate of its own in place of a crate of
crates.io, in every build of the workspace: `[patch.crates-io]`. The
crate in place must have the same name and a version the dependents
accept. A scratch workspace showed that such a crate may itself depend
on the crate it replaces: a crate `snap` 1.1.3 of a path, which depends
on `snap = "=1.1.2"` of crates.io under another name, builds with
`parquet` 58.4.2, which then calls the path crate.

## Goals / Non-Goals

**Goals:**

- Decode Snappy in about DuckDB's cycles on the pages of strings, with
  no change to the `parquet` crate or to Tessera's reader.
- The same bytes as `snap` on every valid stream, and an error on every
  stream it refuses.
- No stream, valid or not, makes the decoder read or write outside its
  buffers, or panic; shown by proofs, Miri, a fuzz target and tests.
- Own only the decoder: nothing of `snap` is copied.

**Non-Goals:**

- A compressor of our own: the real `snap`'s stays.
- The frame format of `snap` (`snap::read`, `snap::write`): the crate
  in place does not give it. No crate the workspace builds uses it; one
  that did would fail to build, so the gap cannot go unseen.
- A choice of the SSSE3 shuffle while running, on builds of x86-64
  that do not target SSSE3, and a count on x86-64.
- The zeros the codec writes before decoding, one to two per cent of
  the profiles: they belong to the `parquet` crate.

## Decisions

### A crate in place of `snap`, not a fork

- **Was:** `parquet` calls `snap` 1.1.2 of crates.io.
- **Will be:** `parquet` calls `crates/snap`, a crate of the workspace
  named `snap`, version 1.1.3, through `[patch.crates-io]` in the
  workspace's `Cargo.toml`. It depends on `snap = "=1.1.2"` under the
  name `upstream`, and gives:

  ```text
  snap::Error, snap::Result      upstream's
  snap::raw::Encoder             upstream's
  snap::raw::max_compress_len    upstream's
  snap::raw::decompress_len      upstream's
  snap::raw::Decoder             its own
  snap::reference::Decoder       upstream's, for the comparison of the
                                 two in the tests and the counts
  ```

  1.1.3 is a version that `parquet`'s requirement, `1.0`, accepts and
  the dependency on the real crate, `=1.1.2`, does not, so the two
  never meet.
- **Why not a fork.** A fork copies `snap`'s 2 500 lines, which we
  would format, lint, mutate and keep in step with its releases, to
  change one file of them. The crate in place reuses them as they are,
  and the real decoder stays at hand as the oracle of every test.
- **Why not Tessera's own decompression.** The `parquet` crate has an
  entry, `ParquetRecordBatchReader::try_new_with_row_groups`, that reads
  pages a caller gives, so the reader could decompress them itself. That
  entry takes no policy of row selection, which the reader now sets to
  `Selectors`; the reader would hand the crate the page index itself
  and check each page's sizes itself: two hundred lines in the reader
  against none.
- **Why not Google's C++.** The maintainer chose Rust. C++ would bring
  a compiler of C++ and its standard library into the extension, and
  DuckDB's own note says that snappy 1.2 is faster when built by clang
  and slower when built by GCC, the compiler of Linux by default. Miri,
  Kani and the mutants do not read C++.
- **A patch that stops applying.** If a release of `parquet` asked for
  a version 1.1.3 does not satisfy, Cargo would build the real `snap`
  and only warn. CI fails instead: `Cargo.lock` must hold no
  `[[patch.unused]]`, and `cargo tree -i snap@1.1.3` must show
  `parquet` above it.

### The decoder: snappy 1.2.1's loop in Rust

A Snappy stream is its decompressed length as a varint, then elements.
Each element starts with a tag byte whose low two bits give its kind:
a literal, whose bytes follow, or a copy of bytes already written, at
an offset of 11, 16 or 32 bits back. The port follows Google's
decompression of a whole buffer:

```text
  input   [len][tag|data][tag|data] ...                [tag|data]
           |   '--------- fast loop --------------''--careful--'
           v
  output  [.......................................][...........]
                                           64 bytes of room left

  fast loop, two elements a turn, while 130 bytes of input remain
  and 64 bytes of output room:
    entry = TABLE[tag]          the length and the kind's offset bits
    one comparison              literal or copy, no branch on the kind
    the element before          copied now, as 64 bytes at once
    this element                deferred to the next turn
  careful loop: element by element, every length and offset checked
```

- **The table.** 256 entries, one a tag, each the element's length and
  the mask of its offset's bytes: a literal, a copy of 1 byte of offset
  or of 2. A long literal and a copy of 4 bytes of offset leave the fast
  loop for the careful one.
- **Copies of a short pattern.** A copy whose offset is less than its
  length repeats a pattern of up to 16 bytes. It is filled by a shuffle
  of the pattern's bytes through a table of masks: NEON's `vqtbl1q_u8`
  on AArch64, SSSE3's `_mm_shuffle_epi8` on a build that targets SSSE3,
  plain code otherwise, as Google's code does.
- **Room past the end.** The fast loop writes up to 64 bytes past an
  element's end, which a later element overwrites. It runs only while
  64 bytes of output room remain, so it never writes past the buffer;
  the rest is the careful loop's. The `parquet` codec's buffer is the
  page's size exactly, and needs no room of its own.
- **Hints to the compiler.** Google's code carries three hints for
  clang in inline assembly, around the loading of a tag. The port
  starts without them. Each is added, with `core::arch::asm!`, only
  where the machine code of the loop shows the branch or the extra
  instruction it removes.
- **Errors.** A stream the decoder cannot decode returns a variant of
  `snap::Error`: a bad header, a length that does not match, a literal
  or a copy past an end, an offset of zero or before the output's start.
  It may name another variant than `snap` names for the same stream,
  since the loops stop at different elements; `parquet` turns any of
  them into its error, and Tessera into `XX001`.
- **Unsafe code.** The loops read and write through raw pointers. The
  safe entry checks the buffers' sizes first; each `unsafe` block says
  which bound the loop's guard keeps for it.

### The checks

- **Against `snap`.** Property tests: streams that the real encoder
  compresses from data of repeated parts, such as addresses, phrases
  and numbers, and from random bytes; any bytes; valid streams with one
  byte changed. Both decoders give the same bytes, or both an error.
- **Each element.** Tests of every kind of element, of lengths from 1
  to past 64, offsets from 1 to 16 against each length, an offset of
  zero and one before the start, and a stream that ends at each
  element.
- **Kani.** One turn of the fast loop, for any tag bytes and any state
  its guard admits, reads and writes inside the buffers and keeps the
  guard; the copy of a pattern for every offset from 1 to 16; the
  careful loop's checks for any element. The NEON and SSSE3 code is cut
  out under Kani, which reads the plain code, as in
  `tessera-kernels`.
- **Miri.** The crate's tests in the job of Miri, on x86-64: the plain
  code. The NEON code is shown by the same tests on AArch64.
- **Fuzzing.** A target in `crates/snap/fuzz` gives any bytes to both
  decoders under AddressSanitizer, in `fuzz.yml`.
- **Mutants.** The crate's lines are judged by its Rust tests.
- **Machine code.** The fast loop's branches are counted in the
  disassembly of `aarch64-apple-darwin` and `x86_64-unknown-linux-gnu`:
  none on the element's kind.
- **SQL.** The suites, whose files are compressed with Snappy and one of
  whose pages is damaged, and `make crosscheck`.

### The count of the decoders

`cargo clickbench` gains a mode that reads the compressed pages of
named columns of the ClickBench file, about the same amount of each
column from across its groups, and decodes them by `snap::raw::Decoder`
and by `snap::reference::Decoder` into one buffer. It checks that both
give the same bytes, then counts each over all pages several times, in
turns. It counts instructions, cycles, branches and branch misses per
byte with the crate `pmu`, so it runs through `sudo -n`, as
`tessera-bench` does.

## Measurement plan

On the M5 Pro, `hits.parquet` in the page cache.

**The decoders.** The mode above, on `URL`, `Title`, `Referer`,
`SearchPhrase`, `EventTime`, `CounterID`, `UserID` and `WatchID`, about
128 MB of compressed pages each, seven passes, the medians. DuckDB's
counts from the comparison before the change stand beside. The
decoder is kept when it gives the same bytes as `snap` on every page;
its cycles per byte on the four columns of strings are at most 0.65 of
`snap`'s (DuckDB's are 0.45 to 0.61); and on every column of at least
1 MB of pages at most 1.05 of `snap`'s.

**The queries.** `cargo clickbench run --counters --tries 5 --mode on
--workers 0`, one query a run, PostgreSQL 19 with `pg_pmu`, every query
against the commit the work starts from. Cycles decide, by the
maintainer's choice for this change: the decoder does the same work
with fewer branch misses, which instructions do not show. Instructions
and pg_duckdb's cycles at one thread stand beside. Q34 and Q35 spill
and are counted as a pair, the base and the end back to back.

Hypotheses, written before the runs:

- Q28: 15 to 22 per cent fewer cycles;
- Q26: 9 to 14;
- Q21, Q22, Q13, Q25 and the other queries that read `URL`, `Title`,
  `Referer` or `SearchPhrase` in full: fewer cycles;
- queries of numbers only: within 2 per cent of cycles. Their
  instructions may grow, by up to two fifths of the decoder's share;
- the geometric mean over pg_duckdb's cycles: from 1.93 to below 1.8.

The change is kept when Q28 loses at least 12 per cent of its cycles,
Q26 at least 8, and no query gains more than 2. Otherwise the report
says which way failed and why, and the maintainer decides. The report
is kept under `docs/benchmarks/`, as `docs/measuring.md` says.

## Risks / Trade-offs

- [A fault of the port reads or writes past a buffer on a damaged or
  hostile file, in a backend of PostgreSQL] → the bounds proved by Kani
  step by step, the fuzz target under AddressSanitizer against `snap`,
  Miri over the tests, the safe entry's checks of sizes.
- [rustc lays the loop out with branches, and the gain is lost on one
  platform] → the disassembly counted on both targets; hints added
  only where it shows them.
- [A release of `parquet` stops using the crate in place] → CI fails on
  an unused patch; the crate gives exactly the items `parquet` calls,
  so a new one fails the build.
- [The gain on x86-64, Tessera's target, is not counted] → the Mac is
  the machine of the counts; a count on x86-64, with the choice of
  SSSE3 while running, becomes an entry of the roadmap.
- [Instructions grow on columns of numbers] → reported beside the
  cycles; the queries decide by cycles.

## Decisions of the maintainer

- 2026-11-25: Snappy is decoded by Google's loops ported to Rust,
  checked by the repository's proofs and tests, counted by cycles and
  branches, and used in place of `snap`'s decoder.
- 2026-11-25: Not a fork of `snap`, if its own items serve: the crate in
  place reuses them.
- 2026-11-25: Cycles decide the count of the queries of this change;
  instructions stand beside them.
- 2026-11-25: The rule "Only the public C API" of `parquet-fdw`, which
  names the crates of Tessera that the reader links, is read as not
  naming the crate in place: it depends on nothing of Tessera, and the
  rule's check, of the reader's direct dependencies, passes. The pull
  request says so under Architecture.
- 2026-11-25: The proposal is approved.

## Files

- `crates/snap/`: the crate in place, its decoder, its proofs and its
  fuzz target.
- `Cargo.toml`, `Cargo.lock`: the member and the patch.
- `tools/tessera-clickbench/`: the mode of decoders and its README.
- `.github/workflows/ci.yml`, `.github/workflows/fuzz.yml`: the checks.
- `openspec/specs/parquet-fdw/design.md` and `contract.md`, at the
  close.
