# A row's columns without walking it again, runs of 2026-11-02

The runs that the change `deform-without-restarts` cites: a second
cursor a row for the columns behind its cursor, and the earlier of two
compared columns read first.

- **Machine**: Apple M5 Pro, macOS 26.6.2, on AC power; other sessions
  of work shared it (load 2.6 to 4.7), so single runs drift by a few
  percent: the times come as two pairs.
- **PostgreSQL** master (20devel), a release build without assertions;
  TPC-H at SF 1 and the bench cluster at scale 1, serial plans.

## Restarts

`Deformed Datums` and `Restarted Datums` of the lineitem scan, EXPLAIN
(ANALYZE, VERBOSE), before and after the change:

| query | deformed | restarted before | restarted after |
|---|--:|--:|--:|
| Q1 | 41 500 761 | 35 499 546 | 5 916 591 |
| Q6 | 7 272 908 | 1 271 693 | 1 157 533 |
| Q10 | 7 709 495 | 229 410 | 114 705 |
| Q12 | 10 548 182 | 4 515 979 | 2 800 542 |
| Q14 | 6 229 164 | 151 966 | 75 983 |
| Q19 | 7 202 309 | 1 072 723 | 1 072 602 |
| Q21 | 15 952 465 | 6 001 215 | 0 |

Q1 restarts once a row now, for attribute 5 after its filter's 11,
where it restarted for each of 5 to 10. Q6 and Q19 read a column their
second cursor has passed (5 after 7), which restarts it as before.

## Times

[`tpch-pair1.txt`](tpch-pair1.txt) and [`tpch-pair2.txt`](tpch-pair2.txt)
are `cargo tpch compare` of the base `74d7e398` and the change, run
base, change, base, change, 7 pairs a query (runs `tpch-sf1-A7W1YJ`
against `tpch-sf1-fdF97w`, and `tpch-sf1-g5Tz1R` against
`tpch-sf1-e2GMgp`). In both pairs Q1 takes 0.909 to 0.929 of the base's
time, Q21 0.867 to 0.931 and Q12 0.969 to 0.982; Q14 0.991 and 0.995;
the others move both ways with the control, within 4 %.

[`ab-win.md`](ab-win.md) is `cargo ab measure --family win` of the
same code against the base, alternating (run `ab-8GY4DV`): every case
reads one column a filter, the common path, and by its minimum each is
0.97 to 1.02 of the base, none flagged.
