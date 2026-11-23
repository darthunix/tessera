## Why

A batch hands a string column to its consumers only as PostgreSQL
`Datum` values, and a `text` value is a header and a copy of the bytes.
The heap source pays nothing for that: its values point into the pages
it reads. The Parquet source pays for every row a consumer asks: it
copies the value's bytes out of the file's decoded pages behind a
header, after the check of UTF-8 its column asks. A consumer that needs
only the bytes then reads them back: TessAgg hashes a grouping key's
bytes, a kernel counts a string's characters, TessSort makes its sort
word from a key's first bytes.

ClickBench over `hits.parquet`, Tessera on and serial against
pg_duckdb with one thread, counted by `pg_pmu` (stage 3 of
`parquet-fdw`, commit 2c43bdc0), in billions of instructions:

- Q13, `GROUP BY SearchPhrase`: 91.9 against 23.0.
- Q15, `GROUP BY SearchEngineID, SearchPhrase`: 104.7 against 25.1.
- Q26, `ORDER BY SearchPhrase LIMIT 10`: 74.6 against 20.1.
- Q27, `ORDER BY EventTime, SearchPhrase LIMIT 10`: 93.4 against 30.3.
- Q28, `avg(length(URL))` by `CounterID`: 351.2 against 75.4.

Profiles of Q13, Q26 and Q28 (macOS `sample`, one sample a
millisecond, about 6 000 samples each) put a quarter to a third of the
time on making the values: copying the bytes, checking UTF-8, writing
headers. In Q13 the key's hash over those bytes takes another tenth,
once a row, though 90 of the column's 226 chunks are encoded through
a dictionary in every page. In Q26 the core's sort word for
`text` hashes each row's bytes too, to estimate whether the word pays.
Of ClickBench's 43 queries, 15 group by a string; Q34 and Q35 group all
100 million rows by `URL`.

This change takes the roadmap's entry `text-keys-by-bytes`, widened by
the maintainer's word of 2026-11-23 to the string functions and the
order by bytes, in one change.

## What Changes

- A batch may offer a string column in a second form beside its
  `Datum` values: for each row asked, a pointer to the value's bytes
  and their length, and, where the source holds the column through a
  dictionary, the value's index in it and the dictionary's identity.
  An optional operation of the batch's operations gives it. The
  `Datum` values stay the form every batch gives and every consumer
  can read: a batch without the operation, or a column it does not
  offer, is read as today.
- The bytes of the form are those a value of the column would hold:
  the source checks them by the column's rule before it gives them, so
  a string that is not UTF-8 fails the query as it fails it today.
- The Parquet source offers the form for its `text`, `varchar` and
  `bytea` columns, from the pages it decoded, without a copy: plain
  chunks by bytes, chunks encoded through their dictionary by bytes and
  index. A projection passes the form of a column it passes on.
- TessAgg numbers a string key by its bytes, with the hash and the
  equality it uses today, and makes the `text` value once, when a new
  value enters its dictionary of keys; through a source's dictionary it
  numbers each index once. The ungrouped `COUNT(DISTINCT)` of a string
  does the same. Rows written to a spill file have their values made
  for those rows alone.
- The kernels of the string functions (`length`, `char_length`,
  `octet_length`, `LIKE`, `starts_with`, `=`, `<>`, `substr`, `left`,
  `right` and the trims) read a column's bytes where the batch offers
  them; a batch function says by a flag of its registration that it
  reads the form.
- A sort key of a string, under a collation whose order is the order
  of the bytes as PostgreSQL itself decides it (`C`, `POSIX` and the
  builtin provider's), gets its sort word from its first eight bytes
  and breaks ties by comparing bytes,
  with no hash of each row; a top-N sort over the form makes the key's
  values only for the rows that beat the worst it keeps.
- Not in this change: the grouped `COUNT(DISTINCT)`, which the change
  `agg-distinct-spill` takes; `min` and `max` of a string, which in
  ClickBench take only the rows a `LIKE` passes; `regexp_replace`,
  `upper` and `lower`, the core's functions; the heap source, whose
  values already point into its pages; joins by a string; the form
  through TessAppend, over partitions or a `UNION ALL`.

## Capabilities

### New Capabilities

- `string-bytes`: the bytes of a string column in a batch: the form,
  its layout and lifetime, the check of the bytes, the batches that do
  not offer it, the Parquet source's form, and what TessAgg, the
  string kernels and TessSort promise when they read it.

### Modified Capabilities

None. `parquet-fdw` is an open change whose spec is not yet in
`openspec/specs/`: what its batches offer is written in `string-bytes`,
so that its requirements stay as its own change leaves them. The
hash-table's requirement on the SQL types of a key stays true: a string
key still reaches the table as its number in the node's dictionary.

## Impact

- **Public C API:** `include/tessera/batch.h` (the operation and the
  structure of the bytes form); `include/tessera/function.h` (a flag of
  a batch function that reads the form, and a field of its call);
  `include/tessera/text.h` (the string kernels' arguments take the
  form).
- **Code:** `runtime/project.c` and `runtime/expr.c` (passing and
  binding the form); `parquet/node.c` and `crates/tessera-parquet`
  (the form from the window's arrays, kept until the batch is released);
  `nodes/agg_keydict.c`, `nodes/agg_group.c`, `nodes/agg.c` (keys and
  `COUNT(DISTINCT)` by bytes); `nodes/key_kind.h` (whether a collation
  orders by bytes); `nodes/sort.c`, `nodes/sort_topn.c`,
  `nodes/sort_planner.c` (the sort key by bytes); `kernels/text.c`,
  `crates/tessera-capi` (the kernels over the form).
- **Guides:** `docs/batch.md`, `docs/kernels.md`, `docs/nodes.md`,
  `docs/source.md`.
- **Checks:** Kani proofs in `tessera-parquet` and `tessera-kernels`,
  the reader's entry point under Miri, cases for CI's sanitizers,
  self-checks of an assert build in TessAgg, TessSort and the source,
  and `tools/tessera-crosscheck` over strings and Parquet tables; the
  capability's contract, `contract.md` of this change.
- **Settings:** none.
- **SQL suites:** `parquet`, the suites of TessAgg and TessSort, and a
  test source in `test/` that offers the form, so that the consumers
  are tested without the Parquet module.
- **Architecture:** the contract of `parquet-fdw` forecasts this
  change in `tessera/batch.h`, the node's batches and TessAgg, and not
  in the reader; the form's pointers into the reader's arrays need an
  entry point of the reader's crate, a departure the design explains.
  The contract of `source-scan` forecasts it in the batch contract, the
  expression compiler and the kernels' inputs.
- **Depends on:** `parquet-fdw`; the branch starts from its branch.
  Its stage 4, the 15 queries that group by a string, is this change's
  base.
