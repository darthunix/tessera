# Design

## Context

The `parquet` crate chooses a page's decompression itself. Its page
reader, `SerializedPageReader`, makes a codec from the column chunk's
compression when it is built. For Snappy the codec is twenty lines
around the crate `snap`: it sizes the page's output from the page's
header, fills it with zeros, and calls `snap::raw::Decoder`. The
builder of the Arrow reader that Tessera uses,
`ParquetRecordBatchReaderBuilder`, has no way to give it another.

The crate has a lower entry, `try_new_with_row_groups` of
`ParquetRecordBatchReader`. It reads the pages that a caller's
`RowGroups` gives, a page reader for each column chunk. A page reader
built over a chunk's metadata marked uncompressed hands the pages over
as the file stores them, compressed. This entry takes the levels of the
columns, the batch size and the selection of rows, but no policy of
row selection: it reads by the crate's default, `Auto` with a
threshold of 32. That policy reads a selection whose runs average 32
rows or more by skipping and reading runs, as the reader's `Selectors`
does now, and a selection of shorter runs by a mask: every row of the
span decoded, the unselected dropped. Tessera loads no page index, so
neither policy skips a page by its index.

A crate of the workspace in place of `snap`, through Cargo's
`[patch.crates-io]`, was tried first. Such a crate cannot depend on the
real `snap` of crates.io: the two are versions of one crate from one
source, which Cargo resolves for the whole workspace but refuses for
`cargo build -p tessera-parquet`, the build `make` runs.

## Goals / Non-Goals

**Goals:**

- Decode Snappy in about DuckDB's cycles on the pages of strings, with
  no change to the `parquet` crate.
- The same bytes as `snap` on every valid stream, and an error on every
  stream it refuses.
- No stream, valid or not, makes the decoder read or write outside its
  buffers, or panic; shown by proofs, Miri, a fuzz target and tests.
- The same rows and errors by every path of the reader.

**Non-Goals:**

- A compressor: the files the tests write are compressed by `parquet`'s
  writer and `snap`, as now.
- A choice of the SSSE3 shuffle while running, on builds of x86-64
  that do not target SSSE3, and a count on x86-64.
- The zeros the output of a page is filled with before decoding, one to
  two per cent of the profiles: the decoder writes into initialized
  bytes, as `snap` does.
- Other codecs: the reader refuses them, as now.

## Decisions

### The reader's own pages

- **Was:** the reader builds each read with
  `ParquetRecordBatchReaderBuilder` over the shared file, for one group
  and a selection, with the policy `Selectors`; the crate's page reader
  decompresses each page by `snap`.
- **Will be:** the reader builds each read with
  `try_new_with_row_groups`, over a `RowGroups` of its own: one group,
  and for each column chunk a page reader of its own.

  ```text
  ParquetRecordBatchReader              the crate's
    '- the reader's RowGroups           one group of rows
         '- the reader's page reader    a Snappy chunk:
              |                           pages decompressed by the
              |                           reader's decoder
              '- SerializedPageReader   the crate's, over the chunk's
                                        metadata marked uncompressed
  ```

  A chunk without compression keeps the crate's page reader as it is.
  The dictionary page that the reader reads for a column's keys goes
  through the same page reader.
- **What it costs.** The policy of row selection becomes the crate's
  default. A selection of runs shorter than 32 rows on average is read
  by a mask: more rows decoded, the same rows returned. The first step
  of the work changes only this, with `snap` still decoding, and is
  counted on the queries whose clauses the source checks.
- **The size in a page's header.** With the chunk marked uncompressed,
  the crate's page reader does not give a page's decompressed size from
  its header, so it is not compared with the size the stream's own
  header gives. The decoder writes the stream's size, or fails; the
  crate's decoders of levels and values check the lengths they read, as
  they do for a page without compression.
- **Why not a crate in place of `snap`.** Cargo refuses it, as the
  context says. The real `snap` from another source, a copy in the
  repository or a dependency of git, would let it build; the maintainer
  chose the reader's own decompression instead.
- **Why not a fork of `snap`.** A fork copies its 2 500 lines, which we
  would format, lint, mutate and keep in step with its releases, to
  change one file of them.
- **Why not Google's C++.** The maintainer chose Rust. C++ would bring
  a compiler of C++ and its standard library into the extension, and
  DuckDB's own note says that snappy 1.2 is faster when built by clang
  and slower when built by GCC, the compiler of Linux by default. Miri,
  Kani and the mutants do not read C++.
- **Why a module of `tessera-parquet`, not a crate.** The decoder
  serves the reader alone. A crate of the workspace that the reader
  linked would be a second crate of Tessera under the reader, which the
  rule "Only the public C API" of `parquet-fdw` refuses. In the
  reader's crate, its proofs, Miri and mutants run with the crate's.

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

- **The careful loop.** Safe Rust: each element's bytes are taken by
  slices whose bounds are checked, and a copy of a short pattern is
  made by copies of the bytes already repeated, each twice the last.
  It decodes a whole stream alone, and the rest of a stream after the
  fast loop.
- **The table.** 256 entries, one a tag, each the element's length and
  the mask of its offset's bytes: a literal, a copy of 1 byte of offset
  or of 2. A long literal and a copy of 4 bytes of offset leave the fast
  loop for the careful one.
- **Copies of a short pattern in the fast loop.** A copy whose offset
  is less than its length repeats a pattern of up to 16 bytes. It is
  filled by a shuffle of the pattern's bytes through a table of masks:
  NEON's `vqtbl1q_u8` on AArch64, SSSE3's `_mm_shuffle_epi8` on a build
  that targets SSSE3, plain code otherwise, as Google's code does.
- **Room past the end.** The fast loop writes up to 64 bytes past an
  element's end, which a later element overwrites. It runs only while
  64 bytes of output room remain, so it never writes past the buffer;
  the rest is the careful loop's. The output is the page's size
  exactly, and needs no room of its own.
- **Hints to the compiler.** Google's code carries three hints for
  clang in inline assembly, around the loading of a tag. The port
  starts without them. Each is added, with `core::arch::asm!`, only
  where the machine code of the loop shows the branch or the extra
  instruction it removes.
- **Errors.** A stream the decoder cannot decode gives an error that
  names what failed: the header, a literal or a copy past an end, an
  offset of zero or before the output's start, or elements that end
  short of the length. The page reader passes it to the crate as its
  error, and the reader raises `XX001`, as for any page it cannot
  decode.
- **Unsafe code.** The fast loop reads and writes through raw pointers.
  The safe entry checks the buffers' sizes first; each `unsafe` block
  says which bound the loop's guard keeps for it.

### The checks

- **Against `snap`.** Property tests: streams that `snap`'s encoder
  compresses from data of repeated parts, such as addresses, phrases
  and numbers, and from random bytes; any bytes; valid streams with one
  byte changed; streams of elements written by hand, valid and not.
  Both decoders give the same bytes, or both an error.
- **Each element.** Tests of every kind of element, of lengths from 1
  to past 64, offsets from 1 to 16 against each length, an offset of
  zero and one before the start, and a stream cut at each byte.
- **Kani.** One turn of the fast loop, for any tag bytes and any state
  its guard admits, reads and writes inside the buffers and keeps the
  guard; the copy of a pattern for every offset from 1 to 16. The NEON
  and SSSE3 code is cut out under Kani, which reads the plain code, as
  in `tessera-kernels`.
- **Miri.** The decoder's tests in the job of Miri, on x86-64: the
  plain code. The NEON code is shown by the same tests on AArch64.
- **Fuzzing.** A target in `crates/tessera-parquet/fuzz` gives any
  bytes to both decoders under AddressSanitizer, in `fuzz.yml`.
- **Mutants.** The decoder's lines are judged by its Rust tests.
- **Machine code.** The fast loop's branches are counted in the
  disassembly of `aarch64-apple-darwin` and `x86_64-unknown-linux-gnu`:
  none on the element's kind.
- **SQL.** The suites, whose files are compressed with Snappy and one of
  whose pages is damaged, and `make crosscheck`.

### The count of the decoders

`cargo clickbench` gains a mode that reads the compressed pages of
named columns of the ClickBench file, about the same amount of each
column from across its groups, and decodes them by the reader's decoder
and by `snap`'s into one buffer. It checks that both give the same
bytes, then counts each over all pages several times, in turns. It
counts instructions, cycles, branches and branch misses per byte with
the crate `pmu`, so it runs through `sudo -n`, as `tessera-bench` does.

## Measurement plan

On the M5 Pro, `hits.parquet` in the page cache. Cycles decide, by the
maintainer's choice for this change: the decoder does the same work
with fewer branch misses, which instructions do not show. Instructions
stand beside the cycles.

**The reader's own pages.** The first step, with `snap` still
decoding, against the commit the work starts from: `cargo clickbench
run --counters --tries 5 --mode on --workers 0`, one query a run,
PostgreSQL 19 with `pg_pmu`, on Q2, Q8, Q13, Q21, Q22, Q26, Q28, Q37,
Q40 and Q41, the queries whose clauses the source checks and those the
change is for. Hypothesis: every one within 2 per cent of its cycles,
since the same pages are decoded by the same code. A query past 2 per
cent stops the work: the report shows which policy its selection took,
and the maintainer decides.

**The decoders.** The mode above, on `URL`, `Title`, `Referer`,
`SearchPhrase`, `EventTime`, `CounterID`, `UserID` and `WatchID`, about
128 MB of compressed pages each, seven passes, the medians. DuckDB's
counts from the comparison before the change stand beside. The
decoder is kept when it gives the same bytes as `snap` on every page;
its cycles per byte on the four columns of strings are at most 0.65 of
`snap`'s (DuckDB's are 0.45 to 0.61); and on every column of at least
1 MB of pages at most 1.05 of `snap`'s.

**The queries.** The same command on every query, against the commit
the work starts from, with pg_duckdb's cycles at one thread beside.
Q34 and Q35 spill and are counted as a pair, the base and the end back
to back.

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
- [The default policy reads a selection of short runs by a mask, with
  more rows decoded] → counted in the first step, before the decoder
  changes; the queries' threshold covers it at the end.
- [A page whose header's size disagrees with its stream is read by its
  stream, where the crate refused it] → the crate's decoders of levels
  and values check every length; the suite's damaged page still raises
  `XX001`.
- [A release of `parquet` changes the low entry or the page reader]
  → the reader's own pages are a few dozen lines over public items; a
  change of them fails the build.
- [The gain on x86-64, Tessera's target, is not counted] → the Mac is
  the machine of the counts; a count on x86-64, with the choice of
  SSSE3 while running, becomes an entry of the roadmap.
- [Instructions grow on columns of numbers] → reported beside the
  cycles; the queries decide by cycles.

## Decisions of the maintainer

- 2026-11-25: Snappy is decoded by Google's loops ported to Rust,
  checked by the repository's proofs and tests, counted by cycles and
  branches, and used in place of `snap`'s decoder.
- 2026-11-25: Not a fork of `snap`.
- 2026-11-25: Cycles decide the count of the queries of this change;
  instructions stand beside them.
- 2026-11-25: The proposal is approved.
- 2026-11-25: No crate in place of `snap`: Cargo refuses one that
  depends on the real `snap` of crates.io, and the maintainer chose the
  reader's own decompression over taking `snap` from another source.

## Files

- `crates/tessera-parquet/src/pages.rs`: the reader's `RowGroups` and
  page reader.
- `crates/tessera-parquet/src/snappy.rs` and its tests and proofs: the
  decoder.
- `crates/tessera-parquet/src/reader.rs`, `src/dictionary.rs`: the
  reads through the reader's pages.
- `crates/tessera-parquet/fuzz/`: the fuzz target.
- `tools/tessera-clickbench/`: the mode of decoders and its README.
- `.github/workflows/ci.yml`, `.github/workflows/fuzz.yml`: the checks.
- `openspec/specs/parquet-fdw/design.md` and `contract.md`, at the
  close.
