## 0. How the work goes

The branch `text-keys-by-bytes` starts from `parquet-fdw`'s branch. Its
first pull request, #172, the proposal's, took the proposal and group 2,
and the maintainer merged it into `parquet-fdw` on 2026-11-23; the rest
goes in pull requests of its own into that branch until `parquet-fdw` is
merged, in four stages: the form and the Parquet source's form (groups 2
and 3), TessAgg's keys (4), the kernels (5), the sort (6). Every task is
one commit, but 7.3, the pull request's description; each is reviewed by
Codex and Claude Fable before the next starts. Each commit passes
`.github/scripts/check-specs.sh` and the check named in its task, and
names in a scenario's "Verified by" or a contract's "Checked by" each
test or check it lands. "The suites" means `make && make install` and
`make installcheck
EXTRA_REGRESS_OPTS="--temp-instance=$PWD/test/tmp_check"` on PostgreSQL
master, 19 and 18 built as CI builds them, with assertions. A count is
taken by `cargo clickbench run --counters` and `cargo ab measure
--counters` and judged by its instructions, as the design's measurement
plan says; by the maintainer's word of 2026-11-23 it needs no idle
machine, which a timed run still waits for. A commit that changes a hot
path carries in its message its counts against the commit before it; the
last task of each stage commits the stage's report. A Kani harness is
bounded to finish within CI's 600 seconds; what its bounds leave out is
named in the harness and covered by a property test.

## 1. The proposal

- [x] 1.1 Write the proposal, the spec delta, the design, the tasks
      and the contract of this change; in the roadmap, the entry
      `text-keys-by-bytes` taken and widened to the string functions
      and the order by bytes. Check: `openspec validate
      text-keys-by-bytes --strict`; `.github/scripts/check-specs.sh`

## 2. The form

- [x] 2.1 `include/tessera/batch.h`: `TessBytesColumn`, the optional
      operation `get_bytes_column` and a helper that asks a batch for
      the form and answers false for a batch without the operation; the
      offsets of `TessBytesColumn` asserted in the installed headers'
      check; `docs/batch.md`. Check: the suites
- [x] 2.2 `include/tessera/bridge.h`, `bridge/`: the optional operation
      of `TessApi` that gives a dictionary's identity from one counter
      of the backend, 64-bit, from 1, with an error should it ever pass
      its last value; a test module call that takes identities from two
      modules and finds them distinct. Check: the suites
- [x] 2.3 `test/`: a test source that gives a table of strings in both
      forms, with and without dictionaries, and counts the asks of each
      form; a test module call that reads the form and checks the
      layout, the dictionary's count of values, the lifetime of every
      pointer across later asks of the batch, and a read of an old
      structure of the form's size before a field, allocated at that
      size. Check: the suites
- [x] 2.4 `runtime/project.c`: a projection's batch offers the form of
      a column it passes on unchanged, and answers false for a column
      it computes; a case of the test source through a projection.
      Check: the suites

## 3. The Parquet source's form

- [x] 3.1 `crates/tessera-parquet`: an entry point that gives, for the
      rows asked of the window, each row's pointer, length and index,
      the dictionary's count of values, and a number of the reader for
      each new values array, and keeps a reference to the window's
      array until the caller drops it; a Kani proof, within its bounds,
      that every pointer and length it gives from offsets and indexes
      it checked lies inside its buffer; tests, run under Miri, that
      read every pointer given after the window is replaced, after the
      reader's own pending arrays are dropped and after other reads,
      while the reference lives; Miri's job of CI runs them. Check:
      `make rust-check`; `cargo kani -p tessera-parquet`; `cargo miri
      test -p tessera-parquet --lib c::tests`; `cargo mutants` over the
      changed lines
- [x] 3.2 `crates/tessera-parquet`: the check of the bytes given by the
      column's rule: a dictionary's values array whole once; a plain
      window's encoding and zero bytes in one pass, once a window; every
      other case, varchar of a length among them, read row by row
      through the plan, so that a row fails with the error its `Datum`
      would; in a debug build, every window checked the cheaper way read
      row by row again; property tests against `datum-arrow`'s check
      over windows, masks, `NULL` rows, checked and trusted text,
      `bytea` and lengths of `varchar(n)`, and a unit test of the size's
      limit that allocates no value of that size; a Kani proof, within
      its bounds, that the one pass passes exactly where each value's
      check of encoding and zero bytes passes. Check: `make rust-check`;
      `cargo kani -p tessera-parquet`; `cargo mutants` over the changed
      lines
- [x] 3.3 `parquet/node.c`: `get_bytes_column` for the `text`,
      `varchar` and `bytea` columns; the identity of each new values
      array of a reader asked of `TessApi`; the references of a batch
      dropped when its memory context is reset; `EXPLAIN (ANALYZE)`'s
      count of values made counts `Datum`s alone; in an assert build,
      the rows given as bytes checked again row by row by the plan;
      cases of the `parquet` suite, through a test module's call that
      asks the form, over plain and dictionary chunks, `NULL` rows,
      empty strings, bad bytes in and out of the mask and `varchar(n)`
      too long, serial (a parallel plan's workers read the form first
      in 4.1, through TessAgg's partial stage); `docs/source.md`; the
      forecast of `parquet-fdw`'s contract updated, and the roadmap's
      entry for simplifying the reader's boundary with the node. Check:
      the suites
- [x] 3.4 `test/tessera_parquet_test.c`: a call that asks a batch for a
      column as bytes, keeps every pointer, has the reader replace the
      window's arrays, rescans, then reads every byte through those
      pointers before the batch is released; since the node reads a
      batch's window only once, an injection point of the node makes
      each ask read it again, behind the reader; an injection point
      after the references are taken and before the release, which
      raises an error, and a count of the references alive, which the
      reader keeps for the backend, back to where it was after a
      rescan, the scan's end and the error; cases of the
      `parquet_cancel` suite over them, which runs where injection
      points are; the commit's message shows that a mutant that drops a
      batch's reference early fails under ASan, and one that never
      drops it fails the counts. Check: the suites; the suites under
      ASan and UBSan as CI's job runs them
- [x] 3.5 `tools/tessera-crosscheck`: a cluster of `UTF8`; string
      columns; the collations `C`, `C.UTF-8` of the builtin provider, a
      libc one and a nondeterministic one; for a query with `ORDER BY`,
      the rows compared in order, the order made total by every output
      column; for a grouping under a nondeterministic collation, groups
      compared by their counts, not by which value stands for them;
      errors left to the suites. Check: `make crosscheck`, within the
      time CI gives it
- [x] 3.6 `tools/tessera-crosscheck`: the extension `tessera_parquet`,
      and for each table of strings a Parquet twin, its rows read back
      from the heap table and written by the crate's writer in
      dictionary, plain and mixed chunks; each query over the twin
      compared with the same query over the heap table, with groupings,
      `ORDER BY … LIMIT`, `LIKE` and `length`. Check: `make
      crosscheck`, within the time CI gives it
- [x] 3.7 Count as the measurement plan says for the form: every query
      counted no more than 1 per cent above the base; the report under
      `docs/benchmarks/`. Check: `.github/scripts/check-specs.sh`

## 4. TessAgg's keys

- [x] 4.1 `nodes/agg_keydict.c`, `nodes/agg_group.c`: the lookup of a
      key by bytes beside the lookup by `Datum`, with the same hash and
      equality; a value copied from its bytes when it enters; the
      values of rows written to a spill file made from their bytes for
      those rows alone; the read of the columns in their order without
      the columns read as bytes; cases over the test source and the
      `parquet` suite, grouped by `text`, `varchar` with a number of a
      column before it and `bytea`, a nondeterministic collation, a
      serial spill by one key and by two; `docs/nodes.md`. Check: the
      suites; `make crosscheck`
- [x] 4.2 The array from a source dictionary's index to a key's number
      and hash: its size bounded by the dictionary's count of values
      and by a share of the grouping's memory, its arithmetic checked,
      every index checked against it before it is read, the lookup by
      bytes past its share; dropped when the identity changes, when the
      key dictionary is reset, on a spill's replay and when a partial
      stage sends its table up, and at a rescan; in an assert build,
      each hit checked against the group's bytes; cases of the test
      source at the array's edges, with large sparse indexes, past its
      share, and at a rescan with an unchanged identity, and of the
      `parquet` suite with a spill of repeated indexes, a partial stage
      that sends its tables up and a dictionary given again. Check: the
      suites; the suites under ASan and UBSan; `make crosscheck`
- [x] 4.3 The ungrouped `COUNT(DISTINCT)` of a string through the same
      lookups; cases as in 4.1 and 4.2, and one whose argument comes
      after another column it reads. Check: the suites; `make
      crosscheck`
- [x] 4.4 Count the string queries, the controls and the heap's cases
      against the base, as the measurement plan says; the report under
      `docs/benchmarks/`. Check: `.github/scripts/check-specs.sh`

## 5. The kernels

- [x] 5.1 `crates/tessera-capi`: the Rust mirror of `TessBytesColumn`
      with a test of its size and offsets, and a second string source
      of the kernels over it; `include/tessera/text.h`: `TessTextArg`
      with its field `bytes`, and the entry points that take a bare
      column taking a `TessTextArg`, with `kernels/text.c` passing its
      `Datum` columns as before; Rust tests and property tests that
      compare each function over both forms of the same strings; tests
      of the bytes source under Miri, over sparse selections, rows not
      asked left as they are, `NULL` rows, empty strings, row counts and
      both forms of an argument, which Miri's job of CI runs. Check:
      `make rust-check`; the suites; `cargo miri test -p tessera-capi
      --test text`; `cargo mutants` over the changed lines
- [x] 5.2 `include/tessera/function.h`: the flag `TESS_FUNCTION_BYTES`
      and the field `bytes` appended to `TessFunctionCall`, its offsets
      asserted in the installed headers' check, and a test function
      that reads a call allocated at the size before `bytes`;
      `runtime/expr.c` gives the form of a column it reads unchanged to
      a function with the flag; in an assert build, it reads the first
      and the last byte of every row it hands a kernel in the form; a
      test module's functions with and without the flag;
      `docs/function.md`. Check: the suites
- [ ] 5.3 `kernels/text.c`: the flag, the form taken for the kernels'
      own rows, and every row a kernel sends to the core's function
      given a value made from its bytes in the call's memory context,
      in one commit, since a function with the flag must take every row
      it is given; cases of `LIKE` with `_`, an escape and more pieces
      than the matcher holds, of a comparison of a column of bytes with
      a compressed column of the test source, of `sum(length(s))` over
      `(s, n)` read in reverse order, and of each function of the spec
      over the test source and the Parquet table, in an aggregate's
      argument, a filter and an output column; `docs/kernels.md`.
      Check: the suites; `make crosscheck`
- [ ] 5.4 Count as in 4.4. Check: `.github/scripts/check-specs.sh`

## 6. The sort

- [ ] 6.1 `crates/tessera-kernels`: the kernel that makes a string's
      sort word, its first eight bytes, big-endian, zero-padded, its
      top bit flipped, from either form; unit and property tests over
      lengths about eight bytes, equal prefixes, the bytes `00`, `7f`,
      `80` and `ff` and longer suffixes; a Kani proof, within its
      bounds, that the words, the rest and the lengths order two strings
      as their unsigned bytes do. Check: `make rust-check`; `cargo kani
      -p tessera-kernels`; `cargo mutants` over the changed lines
- [ ] 6.2 `nodes/key_kind.h`: whether a collation orders by bytes, from
      PostgreSQL's locale; `nodes/sort.c`, `nodes/sort_planner.c`,
      `nodes/sort_external.c`: a key kind for `bytea` and for a string
      of such a collation under its type's default operator class, its
      word from the kernel, ties by bytes and then length, read from
      the form or the `Datum`; the ordered gather keeps the generic
      kind for a string key; in an assert build, each pair of adjacent
      rows after a sort and after a merge checked by their full bytes,
      and each comparison of ties; cases under `C`, the builtin
      `C.UTF-8`, a libc collation and a test module's operator class,
      with multibyte strings, prefixes, strings that share their first
      eight bytes, `bytea` of `00`, `7f`, `80` and `ff`, `NULL` rows
      first and last, both directions, in memory, spilled and through
      an ordered gather; `docs/nodes.md`. Check: the suites; `make
      crosscheck`
- [ ] 6.3 `nodes/sort_topn.c`: the key's values asked only for the rows
      that beat the worst kept when their batch is read; in an assert
      build, each row it drops checked by its full bytes against the
      worst kept; a case of the `parquet` suite over rows in no order of
      the key, whose count of values made is below the rows read.
      Check: the suites; `make crosscheck`
- [ ] 6.4 Count as in 4.4. Check: `.github/scripts/check-specs.sh`

## 7. The spec, the design and the close

- [ ] 7.1 Every scenario of the spec delta names its test, and every
      pending check of the contract its check. Check: `openspec
      validate text-keys-by-bytes --strict`;
      `.github/scripts/check-specs.sh`
- [ ] 7.2 `openspec/specs/string-bytes/design.md`, written for a reader
      who knows nothing of the part; `## Files` and `## Tests` last;
      the contract moved beside the spec. Check:
      `.github/scripts/check-specs.sh`
- [ ] 7.3 The pull request's description, an index of evidence
      with the counts of 3.7, 4.4, 5.4 and 6.4. Check: CI green
- [ ] 7.4 `openspec archive text-keys-by-bytes --yes`, the folder it
      makes under `openspec/changes/archive/` deleted; the entry
      `text-keys-by-bytes` leaves the roadmap, and what is left becomes
      entries. Check: `.github/scripts/check-specs.sh`
