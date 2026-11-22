## Why

TessAgg folds `sum` and `avg` of `bigint` itself. A kernel adds the
values of a batch into an exact sum of 128 bits, with their count, as
PostgreSQL's `int8_avg_accum` keeps its `Int128AggState`, and the node
makes the `numeric` result once, at the end. But the kernel takes a
value only below 10^18 in magnitude, a value of at most 18 digits. It
leaves each value of 19 digits to the node, which turns the value into
a `numeric` and adds it with `numeric_add`: a call a row, with two
allocations and a free. A `bigint` reaches 9.2 × 10^18, so a column of
large identifiers or hashes takes that path for most of its rows.

A profile on the branch of `parquet-fdw` found it. The query was
ClickBench's query 4, `SELECT avg(UserID) FROM hits`, over the 100
million rows of `hits.parquet`, serial, on an Apple M5 Pro. `UserID` is
a `bigint` near 10^18. Tessera ran about 1075 instructions a row, about
900 after fixes of the scan that do not touch the sum; DuckDB ran 78.
The macOS `sample` tool put 43 to 55 % of the time under the node's
`fast_advance`, in `int64_to_numeric` and `numeric_add` with their
allocations. A heap table takes the same path: none of it is Parquet's
own.

The limit of 18 digits is the rule of a decimal: a `numeric` held as an
`int64` at its scale, which the kernels read, compare and compute with.
An integer sum does not need that rule. Its terms are all at scale 0,
so the sum is never rescaled, and an `i128` holds the sum of 2^64 values
of an `int64`, more than a count of 64 bits can reach.

The maintainer asked for this work on 2026-11-22, apart from the change
`parquet-fdw`. It is work on speed, so it comes as a change with a
measurement plan.

## What Changes

- The integer terms of a sum take every `bigint` value. The values of
  19 digits go into the 128-bit sum with the others: without `GROUP
  BY`, with it, in partial aggregates, and in the rows that go to disk
  past `hash_mem` and come back. A row the kernels take alone, not with
  its word of 64 rows, carries a value of 19 digits as a term of its
  own kind, which the sum adds at scale 0.
- The bound of the sum stays: below 10^36 in magnitude. Past it a row
  still goes to the node's `numeric` sum. An integer sum reaches it
  only after about 10^17 rows, but the bound is the one rule of the
  state's words, which its partial format and its checks share with the
  sums of `numeric`.
- Every check of the sum's magnitude against its bound uses
  `unsigned_abs`. A bulk sum of 19 digits rescaled into a state of
  another scale could land on `i128::MIN`, whose `abs` stays negative
  in a release build; the checks of the states the entry points take
  from C and from bytes let that value pass today.
- The count of a sum stays within an int64's, as C and the core keep
  it: a state past it fails on the way in, the kernels refuse a term
  that would take it past, and the node fails with `bigint out of
  range` where it adds counts, as `count(*)` does. A count past it
  needs 2^63 rows or a damaged partial state; read back by C, it became
  negative.
- The family `anyagg` gets cases of `sum` and `avg` over a `bigint`
  column of 19 digits, each beside a twin of 18 digits: without groups,
  by groups whose keys come in runs, and by groups whose keys follow
  one another, the two paths of the grouped loop.
- The comments of the C API and the documents say which rows a sum
  leaves to its caller.

Not in this change:

- `min` and `max` of `date`, the other half of the entry
  `agg-int128-and-date` on the branch of `parquet-fdw`. That branch
  narrows its entry to them when it meets this one.
- `sum` and `avg` of `integer` and `smallint`: their values never pass
  18 digits, so the kernels already take them all.
- A state of its own for `bigint`, as the core's `Int128AggState`: the
  sum state already is a 128-bit sum and a count.
- A new profile of ClickBench's query 4: the session of `parquet-fdw`
  runs it again once this change is in `main`.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

None. The answers do not change: the sum is exact on either path, and
the result is the `numeric` the core's final functions make. No format
changes: the state's words and the partial format keep their layout and
their range. The C API keeps its functions and types; what changes is
which rows `tess_decimal_sum` and `tess_table_accumulate_sums` leave in
their rest. The kernels' entry points are a surface a spec names, but
no capability describes them yet: as `CONTRIBUTING.md` says of a part
without a folder, they are described in `docs/kernels.md` and the
headers, and this change edits that description. A first requirement
for them here would be a capability of one promise, where a capability
comes whole, in a pull request of its own. The change declares
`skip_specs: true`.

## Impact

- Code: `crates/tessera-kernels`, `decimal.rs` (the term of an integer,
  the sum and the sum state that take it, the plain loop of the terms)
  and `accumulate.rs` (the grouped loop of the terms);
  `crates/tessera-capi`, `c/decimal.rs` (the integers' terms). No C
  code changes, but comments.
- `unsafe`: none added. The loop of the integers' terms is already
  `unsafe`; it loses a condition.
- Headers: the comments of `tess_decimal_sum` in
  `include/tessera/decimal.h` and of `tess_table_accumulate_sums` in
  `include/tessera/table.h`.
- Checks: proofs by Kani of the sum's arithmetic, in a new module
  `crates/tessera-kernels/src/decimal/proofs.rs`, run in CI with the
  crate's other proofs.
- Tests: property tests of both loops against a model, at the edges of
  `int64`, alone and past the groups a word keeps; a test of the bound
  at `i128::MIN`; the
  models of `crates/tessera-kernels/tests/decimal.rs` and
  `tests/table.rs`; one test of `tessera-capi` whose expectation of a
  rest turns round; `test/sql/agg.sql` and `test/sql/parallel.sql`
  with their expected outputs.
- The family `anyagg` and its entry in `bench/pg/README.md`.
- Documents: `docs/nodes.md`, `docs/kernels.md` and `docs/table.md`.
- No setting, format, dependency or SQL behavior changes.
- The roadmap: the entry `agg-int128`, first in the queue; at the close
  it leaves.
