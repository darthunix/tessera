# Benchmark runs

Summaries of full runs of the PostgreSQL-level benchmarks of
[`bench/pg`](../../bench/pg/README.md) and of the queries derived from
TPC-H of [`bench/tpch`](../../bench/tpch/README.md), with the plans of
the cases the README cites, newest first:

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
