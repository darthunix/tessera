# A char(n) scalar trimmed once, runs of 2026-11-02

The runs that the change `bpchar-constant-compare` cites: the text
kernel's equality of a char(n) column with a scalar, the scalar trimmed
once a call and each value compared by its first bytes.

- **Machine**: Apple M5 Pro, macOS 26.6.2, on AC power; other sessions
  of work shared it (load 3.4 to 4.9), so single runs drift by a few
  percent and the times come as pairs.
- **PostgreSQL** master (20devel), a release build without assertions;
  TPC-H at SF 1, serial plans.

## Times

`cargo tpch compare`, base and change run in turn, base, change, base,
change, 7 pairs a query; Q19, Q12 and Q10 compare char(n) columns with
constants, Q1 and Q6 do not and are the controls.

- [`tpch-pair1.txt`](tpch-pair1.txt), [`tpch-pair2.txt`](tpch-pair2.txt):
  against `12e35e04`, the change's base (runs `tpch-sf1-ovb1zb` against
  `tpch-sf1-HQfx5q`, `tpch-sf1-Ik1vVT` against `tpch-sf1-mwsSM6`). Q19
  0.813 and 0.856 of the base's time, Q12 0.841 and 0.864. The machine
  slowed in the second pair's last run: the times without Tessera of
  Q6, Q10 and Q12 moved by 1.11 to 1.21, and Q6, which compares no
  char(n), by 1.174 with them; its share of the core's time moved from
  0.685 to 0.723 only.
- [`tpch-earlier-pair1.txt`](tpch-earlier-pair1.txt),
  [`tpch-earlier-pair2.txt`](tpch-earlier-pair2.txt): the same code
  against `74d7e398`, before `deform-without-restarts` (runs
  `tpch-sf1-NfXjFO` against `tpch-sf1-h2LE29`, `tpch-sf1-joBppX`
  against `tpch-sf1-XKOpkS`): Q19 0.812 and 0.768, Q12 0.760 and 0.859,
  the controls 0.94 to 1.02.

With Tessera on, Q19 takes 0.71 to 0.75 of the core's time where it
took 0.87, and Q12 0.51 to 0.54 where it took 0.68.
