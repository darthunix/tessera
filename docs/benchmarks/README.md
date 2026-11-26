# Benchmark runs

Summaries of full runs of the PostgreSQL-level benchmarks of
[`bench/pg`](../../bench/pg/README.md), of the queries derived from
TPC-H of [`bench/tpch`](../../bench/tpch/README.md) and of the counts
of the kernels' benchmarks of
[`crates/tessera-capi/benches`](../../crates/tessera-capi/benches/README.md),
with the plans of the cases the README cites, newest first:

- [2026-11-26, extremes and keys of the word types,
  `agg-date-min-max`](2026-11-26-agg-date-min-max/README.md): the
  cases of `anyagg` and `wordkey` the change adds and their twins, and
  ClickBench's Q7, Q3 and Q1 over `hits.parquet` at 0 workers, the base
  and the end in turns, against `f708ef0d`, cycles deciding,
  pg_duckdb's beside them; the extremes 44 to 54 per cent fewer cycles
  without groups and 25 with them, `DISTINCT` in them 81 to 91, the
  comparisons, sorts and keys of `time` and `money` 52 to 83, Q7 43 to
  44, the twins and controls within 2; `key_time_distinct`
  unsettled by the plan's spread, and `g_group_minmax_dates` 10.5 per
  cent above its twin, which reads one column less.
- [2026-11-25, page headers through a buffer,
  `parquet-page-headers`](2026-11-25-parquet-page-headers/README.md):
  seven of ClickBench's queries over `hits.parquet` counted at 0
  workers, the base and the end in turns, against `fc6b0208`, cycles
  deciding, pg_duckdb's beside them; Q8 19 to 24 per cent fewer cycles
  in three pairs, unsettled by the plan's spread, Q11 7 to 8, Q2 3 to
  7, the rest within 2 or below their spread; buffers of 256 bytes and
  8 KiB no better than 1 KiB.
- [2026-11-25, the reader's decoder of Snappy,
  `parquet-snappy`](2026-11-25-parquet-snappy/README.md): eight of
  ClickBench's queries over `hits.parquet` counted at 0 workers, the
  base and the end in turns, pg_duckdb's cycles beside them, against
  `d16e43c8`, cycles deciding; Q28 25 per cent fewer cycles, Q26 12 to
  15, Q22 28 to 34, Q8 3 per cent more instructions and up to 2.4 more
  cycles in a clean pair; and the decoder against `snap`'s and DuckDB's
  on the file's pages, Apple M5 Pro, PostgreSQL 19.

- [2026-11-24, strings read by views,
  `parquet-string-views`](2026-11-24-parquet-string-views/README.md):
  ClickBench's queries over `hits.parquet` counted at 0 workers, Q34
  and Q35 in pairs, pg_duckdb's cycles beside them, against
  `2f08e7db`; Q28 11.8 per cent fewer instructions, Q26 10.1, Q21 18.2,
  no query more, Apple M5 Pro, PostgreSQL 19.

- [2026-11-24, the sort by bytes, stage 4 of
  `text-keys-by-bytes`](2026-11-24-text-keys-by-bytes-sort/README.md):
  ClickBench's queries over `hits.parquet` counted at 0 workers, Q34
  and Q35 in pairs, pg_duckdb's cycles beside them, and the heap's
  families `sort` and `anykey`, against `9c76fd3e`; Q26 12.4 per cent
  fewer, short of the plan's 15, and its profile, Apple M5 Pro,
  PostgreSQL 19.

- [2026-11-24, the string functions by bytes, stage 3 of
  `text-keys-by-bytes`](2026-11-24-text-keys-by-bytes-functions/README.md):
  ClickBench's queries over `hits.parquet` counted at 0 workers, Q34
  and Q35 in a pair, pg_duckdb's cycles beside them, and the heap's
  families `sort`, `anykey` and `rowwise`, against `f8ee89ef`; Q28 28
  per cent fewer, the four tries of the way there, the heap's `LIKE`
  1.1 per cent more, Apple M5 Pro, PostgreSQL 19.

- [2026-11-24, the Parquet reader's bytes form written in
  place](2026-11-24-parquet-reader-allocs/README.md): the strings'
  queries of ClickBench over `hits.parquet` counted at 0 workers, the
  others as controls, Q34 and Q35 in a pair, against `a3420684`; a
  first try whose loop over rows called out of line, Apple M5 Pro,
  PostgreSQL 19.

- [2026-11-24, TessAgg's keys by bytes, stage 2 of
  `text-keys-by-bytes`](2026-11-24-text-keys-by-bytes-keys/README.md):
  ClickBench's queries over `hits.parquet` counted at 0 workers, Q34
  and Q35 in pairs, and the heap's families `sort` and `anykey`,
  against `76d43940`; Q13's miss of the plan's 15 per cent, the first
  count's two excesses and their fixes, Apple M5 Pro, PostgreSQL 19.

- [2026-11-23, the bytes form, stage 1 of
  `text-keys-by-bytes`](2026-11-23-text-keys-by-bytes-form/README.md):
  ClickBench's 43 queries over `hits.parquet` counted at 0 workers, and
  the heap's families `sort` and `anykey`, against `531b5d7d`; the
  first count's excess and its two fixes, Apple M5 Pro, PostgreSQL 19.

- [2026-11-23, DISTINCT sets past the
  limit](2026-11-23-agg-distinct-spill/README.md): the cycles of the
  new cases of `spill`, of the path that fits against `main` and of
  ClickBench's Q9 and Q10, Apple M5 Pro, PostgreSQL 20devel and 19rc2,
  the change `agg-distinct-spill` against 75fb4ba4.
- [2026-11-22, the sums of `bigint` values of 19
  digits](2026-11-22-agg-int128/README.md): the counts of the family
  `anyagg` with its cases of 19 digits and their twins of 18, the
  times of Tessera on and off, and the machine code of the kernels,
  Apple M5 Pro, PostgreSQL 20devel, the change `agg-int128` against
  `624937e9`.
- [2026-11-22, ClickBench over Parquet, stage
  2](2026-11-22-clickbench-stage2/README.md): the 17 queries that read
  no string, over ten of its files and over `hits.parquet`, Tessera on
  and pg_duckdb at 0 and 8 workers, Tessera off over the ten files, and
  the cycles and instructions of Tessera and DuckDB, Apple M5 Pro,
  PostgreSQL 19, the change `parquet-fdw` at `7d3fdf4c`.
- [2026-11-22, ClickBench over Parquet, stage
  1](2026-11-22-clickbench-stage1/README.md): Q1 to Q7 over ten of its
  files and over `hits.parquet`, Tessera on and off and pg_duckdb at 0
  to 8 workers, and the cycles and instructions of Tessera and DuckDB,
  Apple M5 Pro, PostgreSQL 19, the change `parquet-fdw` at `256e0df5`.
- [2026-11-20, row counts checked on whole words of the
  aggregates](2026-11-20-row-counts/README.md): the counts of
  `aggregate_int32` and `aggregate_int64` of two places of the check,
  and of the second with every function aligned, the Intel Mac and
  Apple M5 Pro, against `106740d6`.
- [2026-11-03, a walk that keeps the columns read behind
  it](2026-11-03-deform-past-varlena/README.md): TPC-H serial and with
  two workers, the probes of the rule and of the step for a one-byte
  header, the families tax and win, Apple M5 Pro, PostgreSQL master,
  the change `deform-past-varlena` against `06297a9f`.
- [2026-11-03, index lookups by batch in the hash
  join](2026-11-03-batch-index-join/README.md): the family indexjoin,
  the calibration lookupcost, TPC-H serial and with two workers, Apple
  M5 Pro, PostgreSQL master, the change `batch-index-join` against
  `b6ddc7ea`.
- [2026-11-03, inputs the predictor cannot
  learn](2026-11-03-bench-unlearned-inputs/README.md): the pilot of
  the forms and the runs of identical sides of every program whose
  cases drawn at random took a ring of inputs, the Intel Mac and Apple
  M5 Pro, the change `bench-unlearned-inputs`.
- [2026-11-02, the join's key filter first, and for small
  tables](2026-11-02-early-key-filter/README.md): TPC-H Q19 and Q21
  serial and with two workers, the join family, Apple M5 Pro,
  PostgreSQL master, the change `early-key-filter` against `16128ec2`.
- [2026-11-02, a right anti join in
  TessHashJoin](2026-11-02-right-anti-join/README.md): TPC-H Q22 serial
  and with two workers, the join family, Apple M5 Pro, PostgreSQL
  master, the change `right-anti-join` against `4b905b62`.
- [2026-11-02, smallint arithmetic, the date bound and the
  zone](2026-11-02-int2-and-date-bounds/README.md): six cases of the
  family `win` and the counts of `arith_int32`, Apple M5 Pro,
  PostgreSQL master 0497318e442, the change
  `int2-and-date-bounds-in-rust` against c14180a5, and the attempt of
  the zone in rounds it left out.
- [2026-11-02, a char(n) scalar trimmed
  once](2026-11-02-bpchar-constant-compare/README.md): TPC-H Q19, Q12
  and Q10, Apple M5 Pro, PostgreSQL master, the change
  `bpchar-constant-compare` against `12e35e04`.
- [2026-11-02, a row's columns without walking it
  again](2026-11-02-deform-without-restarts/README.md): restarts of the
  deform and times of TPC-H and the family `win`, Apple M5 Pro,
  PostgreSQL master, the change `deform-without-restarts` against
  `74d7e398`.
- [2026-11-02, keys hashed by their
  bytes](2026-11-02-native-key-hashes/README.md): profiles and times on
  battery of TPC-H Q1 and the family `anykey`, Apple M5 Pro,
  PostgreSQL master, the change `native-key-hashes` against `7978d70a`.
- [2026-11-01, partial grouping by dictionary
  keys](2026-11-01-partial-keys/README.md): TPC-H Q1 with two workers
  and the family `anykey`, Apple M5 Pro, PostgreSQL master, the change
  `partial-dictionary-keys` against `ee33f2e4`.
- [2026-11-01, table growth](2026-11-01-table-growth/README.md): the
  times and the memory of how a table in memory grows, the change
  `table-growth-one-owner` against the commit of its cases, Apple M5
  Pro, PostgreSQL master 0497318e442, Tessera a0953475.
- [2026-11-01, spill](2026-11-01-spill/README.md): the counters of the
  spill's kernels and times of its reads of text, the base of later
  comparisons of the spill, Apple M5 Pro, PostgreSQL master
  0497318e442, Tessera at the commit of task 2.12 of `spill-crate`.
- [2026-10-02, TPC-H](2026-10-02-tpch/README.md): the 22 queries derived
  from TPC-H at scale factor 1 (`bench/tpch`), Apple M5 Pro, PostgreSQL
  master 311df1dc, Tessera 1f6a7e7.
- [2026-10-01](2026-10-01/README.md): every family, Apple M5 Pro,
  PostgreSQL master 311df1dc, Tessera 0e27e8e.
