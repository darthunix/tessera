## 0. How the work goes

The branch `parquet-page-headers` starts from `main` after the roadmap's
entries of `roadmap-after-snappy`. The maintainer approves the proposal
before any code. The work goes in one pull request into `main`, reviewed
by Codex and Claude Fable. Every task is one commit, except 4.2, the
pull request's description. Each commit passes
`.github/scripts/check-specs.sh` and the check named in its task. "The
suites" means `make && make install` and `make installcheck
EXTRA_REGRESS_OPTS="--temp-instance=$PWD/test/tmp_check"` on PostgreSQL
master built as CI builds it, with assertions and injection points; CI
runs 19 and 18.

## 1. The proposal

- [x] 1.1 Write the proposal, the design and the tasks of this change.
      Check: `openspec validate parquet-page-headers --strict`;
      `.github/scripts/check-specs.sh`

## 2. The headers through a buffer

- [x] 2.1 `crates/tessera-parquet/src/reader.rs`: `get_read` gives a
      `BufReader` of 1 KiB over `At`; `At` keeps the bytes it reads
      within 12 bytes of its start only. The comments that say what the
      kept bytes are follow: `SharedFile`'s, `capturing`'s and
      `take_captured`'s in `reader.rs`, `Snappy`'s and `open`'s in
      `pages.rs`. Tests, as the design's "Tests" lists them:
      - a header read reaches the file once, by the count of reads;
      - long headers: page statistics written into the headers
        (`set_statistics_enabled(Page)`,
        `set_write_page_header_statistics(true)`,
        `set_statistics_truncate_length(None)`), values of some 600
        and 3 000 bytes, a header's length asserted past 1 024; pages
        read and skipped, Snappy and not, compared with the crate over
        a `std::fs::File`, not over the shared file;
      - a wrong size caught after a read, a peek and a read, peeks and
        a read, a peek, a skip and a read, a skip and a read;
      - the last page of a file; a file cut in a header and in a
        page's data;
      - the tests of the count of reads as they were.
      The message carries the counts of Q8 and Q28 against the base.
      Check: `make rust-check`; the suites; `make crosscheck`

## 3. The count

- [x] 3.1 Count as the design's measurement plan says, the base and the
      end in turns, with pg_duckdb's cycles beside; Q8 and Q28 once
      more with buffers of 256 bytes and 8 KiB. The size stays 1 KiB
      unless one of the others gives Q8 2 per cent fewer cycles and Q28
      no more. Keep the report under `docs/benchmarks/`. Check:
      `.github/scripts/check-specs.sh`

## 4. The close

- [ ] 4.1 `openspec/specs/parquet-fdw/design.md` says how the file is
      read from an offset and why through a buffer; the roadmap loses
      `parquet-page-headers`. The change's folder is deleted. Check:
      `openspec validate --all --strict`;
      `.github/scripts/check-specs.sh`
- [ ] 4.2 The pull request's description, as CONTRIBUTING.md asks of a
      pull request that closes a change.
