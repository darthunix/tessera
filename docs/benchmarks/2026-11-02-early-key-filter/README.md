# The join's key filter first, and for small tables, runs of 2026-11-02

The runs that the change `early-key-filter` cites: a TessFilter of
batch clauses alone checks a hash join's Bloom filter before or after
its clauses, whichever drops more rows first, and a table of fewer than
4096 records gets a filter for the child.

- **Machine**: Apple M5 Pro, macOS 26.6.2, on AC power; other sessions
  of work shared it, so the times come as pairs.
- **PostgreSQL** master (20devel), a release build without assertions.
- **Base and change**: `16128ec2` (the benchmark case alone) and
  `b93dce4c` (the filter's place by windows), the change's commits
  before they moved onto `main`.

## TPC-H

`cargo tpch compare`, SF 1, base and change run in turn, base, change,
base, change, 7 pairs a query. Q19 and Q21 are the queries of the
change: their lineitem scans are TessFilters of batch clauses alone
under joins of 485 parts and 411 suppliers. Q1, Q6 and Q14 are the
controls.

- Serial, [`tpch-serial-pair1.txt`](tpch-serial-pair1.txt) and
  [`tpch-serial-pair2.txt`](tpch-serial-pair2.txt) (runs
  `tpch-sf1-PHyFHT` against `tpch-sf1-vJUU5m`, `tpch-sf1-bwQt3f`
  against `tpch-sf1-aOm67Q`): Q19 0.275 and 0.278 of the base's time,
  Q21 0.682 and 0.675; with Tessera on, Q19 takes 0.19 of the core's
  time where it took 0.70, Q21 0.48 to 0.49 where it took 0.72. The
  controls stay within 1.00 to 1.06; Q6, which joins nothing, moved
  1.03 and 1.06 with them, the machine's drift.
- With two workers, [`tpch-w2-pair1.txt`](tpch-w2-pair1.txt) and
  [`tpch-w2-pair2.txt`](tpch-w2-pair2.txt) (runs `tpch-sf1-tpwGoW`
  against `tpch-sf1-0KQg8E`, `tpch-sf1-pIGx1N` against
  `tpch-sf1-s3HTNf`): Q19 0.443 and 0.458, Q21 0.802 and 0.814. The
  controls stay within 0.97 to 1.03.

## The join family

[`ab-join.md`](ab-join.md): `cargo ab measure` over seven cases of
`bench/pg/join.sql` at scale 1, two runs a side alternating (runs
`pg-join-oLSog3`, `pg-join-asAdOS` against `pg-join-YvhDvP`,
`pg-join-sZt2wC`). `filter_first`, a filter that keeps two rows in five
ahead of a clause on f1 that keeps the first 20 000 rows of the fact
table and none after, takes 0.992 of the base's time by its least run
and 0.984 by its median. The other cases stay within 0.94 to 1.01 by
both.

## The rule's tries

The same cases measured the rules before this one:

- The filter always first: `filter_first` 1.35 (runs `pg-join-fI7wAQ`,
  `pg-join-cxh61i` against `pg-join-iIexDo`, `pg-join-jFRid3`), Q19
  0.255 to 0.262 serial.
- One decision after the first 4096 rows: `filter_first` 1.37 (runs
  `pg-join-dd1OMY`, `pg-join-blSPQ2` against `pg-join-jNq1Q4`,
  `pg-join-uk4GIc`): the clause on f1 kept every row of the first ones.
- A window of 4096 rows of the first side alone: `filter_first` 0.99
  (runs `pg-join-OFDBOA`, `pg-join-AyYTaw` against `pg-join-is2i6V`,
  `pg-join-XwSlEO`), but Q19 0.535 and 0.544 serial: behind the filter
  the clauses saw about 10 rows a window, and the order turned back and
  forth.
- A window that waits for both sides, before the rows of a shared
  filter not yet ready were left out of it: `filter_first` 1.000 and
  1.024 (runs `pg-join-gwH55A`, `pg-join-w5QuRa` against
  `pg-join-LtohAn`, `pg-join-yrUSX0`), Q19 0.267 to 0.276 serial and
  0.456 with two workers.
