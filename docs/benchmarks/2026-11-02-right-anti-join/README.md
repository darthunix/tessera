# A right anti join in TessHashJoin, runs of 2026-11-02

The runs that the change `right-anti-join` cites: TessHashJoin taking
the core's right anti join, the plan of `NOT EXISTS` that builds the
side it keeps and probes with the other.

- **Machine**: Apple M5 Pro, macOS 26.6.2, on AC power; other sessions
  of work shared it (load 3.5 to 4.7), so the times come as pairs.
- **PostgreSQL** master (20devel), a release build without assertions.
- **Base and change**: `4b905b62` (the benchmark case alone) and
  `d1250e55` (the node's right anti join), the change's commits before
  they moved onto `8d5fcf7d`, whose smallint and date kernels no query
  measured here reaches.

## TPC-H

`cargo tpch compare`, SF 1, base and change run in turn, base, change,
base, change, 7 pairs a query. Q22 is the query of the change, a `NOT
EXISTS` of 1.5 M orders against 19 000 customers; Q4 (`EXISTS`), Q13
(a left join), Q16 (`NOT IN`) and Q21 (`EXISTS` and `NOT EXISTS`) keep
their plans and are the controls.

- Serial, [`tpch-serial-pair1.txt`](tpch-serial-pair1.txt) and
  [`tpch-serial-pair2.txt`](tpch-serial-pair2.txt) (runs
  `tpch-sf1-H50yyt` against `tpch-sf1-5rwfg5`, `tpch-sf1-6JJnHZ`
  against `tpch-sf1-8DmMvO`): Q22 0.513 and 0.465 of the base's time;
  with Tessera on it takes 0.41 to 0.43 of the core's time where it
  took 0.85. The controls stay within 0.95 to 1.06, with their times
  without Tessera moving as much.
- With two workers, [`tpch-w2-pair1.txt`](tpch-w2-pair1.txt) and
  [`tpch-w2-pair2.txt`](tpch-w2-pair2.txt) (runs `tpch-sf1-geZq1H`
  against `tpch-sf1-a6d0o7`, `tpch-sf1-lCxpZD` against
  `tpch-sf1-mOTp8C`): Q22 0.656 and 0.654, the node's join over its
  shared table where the core's Hash Right Anti Join ran under its
  Gather; with Tessera on 0.63 to 0.64 of the core's time where it took
  0.96 to 0.98. The controls stay within 0.96 to 1.00.

## The join family

[`ab-join.md`](ab-join.md): `cargo ab measure` over six cases of
`bench/pg/join.sql` at scale 1, two runs a side alternating (runs
`pg-join-XPxHPy`, `pg-join-XGIQmC` against `pg-join-9TDDoK`,
`pg-join-pWfiwY`). `right_anti`, `NOT EXISTS` of the 100 000 rows of
the dimension against the 2 M rows of the fact table, takes 0.273 of
the base's time, 25.6 ms where the core's join over the node's scans
took 93.8. The semi, anti, left, right and full joins stay within 2 %,
their times without Tessera moving as much.
