## 0. How the work goes

The branch `parquet-snappy` starts from `main`. The maintainer approved
the proposal on 2026-11-25, before any code. The work goes in one pull
request into `main`, reviewed by Codex and Claude Fable. Every task is
one commit, except 6.2, the pull request's description. Each commit
passes `.github/scripts/check-specs.sh` and the check named in its task.
"The suites" means `make && make install` and `make installcheck
EXTRA_REGRESS_OPTS="--temp-instance=$PWD/test/tmp_check"` on PostgreSQL
master built as CI builds it, with assertions and injection points; CI
runs 19 and 18. "The mode" means `cargo clickbench`'s count of the
decoders of 2.2, run through `sudo -n` by the maintainer. A commit that
changes the decoder carries in its message the mode's counts against
`snap`'s decoder.

## 1. The proposal

- [x] 1.1 Write the proposal, the design and the tasks of this change.
      Check: `openspec validate parquet-snappy --strict`;
      `.github/scripts/check-specs.sh`

## 2. The reader's own pages and the count of the decoders

- [x] 2.1 `crates/tessera-parquet`: the reads through
      `try_new_with_row_groups`, over the reader's `RowGroups` of one
      group and its page reader (`src/pages.rs`), which takes a Snappy
      chunk's pages as stored and decompresses them by the module
      `snappy`, here a call of `snap`'s decoder; the dictionary page of
      the keys through the same page reader. Tests: the pages of
      chunks with and without Snappy, of data pages of both versions
      and of a dictionary, read as the crate's builder reads them; a
      damaged stream an error. The message carries the count of the
      design's first step. Check: `make rust-check`; the suites; `make
      crosscheck`
- [x] 2.2 `tools/tessera-clickbench`: the mode that decodes the pages
      of named columns of the ClickBench file by the reader's decoder
      and by `snap`'s, checks their bytes, and counts instructions,
      cycles, branches and branch misses per byte with `pmu`; its
      section in the README. Tests: the pages of a written file
      collected and decoded by both. Its first run, both decoders still
      the same, shows the mode's noise. Check: `make rust-check`

## 3. The decoder

- [x] 3.1 `crates/tessera-parquet/src/snappy.rs`: the careful loop of
      the design, in safe Rust, decoding whole streams; `snap` a
      dependency of the tests alone; the notice of Google's snappy,
      whose code the module ports. Tests: every kind of element
      against `snap`'s decoder, the lengths and offsets of the design,
      a stream cut at each byte; property tests of valid, random,
      changed and hand-written streams against `snap`'s; in CI's job of
      Miri. Check: `make rust-check`; `cargo miri test -p
      tessera-parquet --lib snappy`; the suites; the mode
- [x] 3.2 `crates/tessera-parquet/src/snappy.rs`: the fast loop of the
      design, two elements a turn with the copy before deferred, with
      the table of tags and the copies of a short pattern by NEON, by
      SSSE3 where the build targets it, and by plain code. Tests: those
      of 3.1 over streams long enough for the fast loop, with its
      edges: 130 bytes of input left, 64 bytes of output room. The
      message carries the count of the fast loop's branches in the
      disassembly of `aarch64-apple-darwin` and
      `x86_64-unknown-linux-gnu`, and the hints of `asm!` it needed.
      Check: `make rust-check`; `cargo miri test -p tessera-parquet
      --lib snappy`; the suites; `make crosscheck`; the mode

## 4. The proofs

- [x] 4.1 `crates/tessera-parquet/src/snappy/proofs.rs`: Kani's proofs
      of one turn of the fast loop and of the copy of a pattern for
      every offset from 1 to 16. Check: `cargo kani -p tessera-parquet`
- [x] 4.2 `crates/tessera-parquet/fuzz`: the target that gives any
      bytes to both decoders, in `fuzz.yml`. The contract of
      `parquet-fdw` gains the rule that no stream makes the decoder
      read or write outside its buffers, checked by the proofs of 4.1
      and this target. The mutants of the decoder's lines, run locally
      on the whole diff. Check: `cargo +nightly fuzz run snappy --
      -max_total_time=300`; `.github/scripts/check-specs.sh`

## 5. The count

- [ ] 5.1 Count as the design's measurement plan says: the mode, then
      every query of ClickBench against the commit the work starts
      from, with pg_duckdb's cycles beside. Keep the report under
      `docs/benchmarks/`. Check: `.github/scripts/check-specs.sh`

## 6. The close

- [ ] 6.1 `openspec/specs/parquet-fdw/design.md` gains the decoder:
      why a crate in place, the loop, the checks, the decisions of the
      maintainer; its contract's forecast of another decompression of
      pages is set right. The roadmap loses `parquet-snappy` and gains
      the count on x86-64 with the choice of SSSE3 while running. The
      change's folder is deleted. Check: `openspec validate --all
      --strict`; `.github/scripts/check-specs.sh`
- [ ] 6.2 The pull request's description, as CONTRIBUTING.md asks of
      a pull request that closes a change.
