# Sparse batches gathered by a consumer's input, runs of 2026-11-04

The runs that the change `gather-sparse-batches` cites: TessHashJoin's
outer side and TessAgg's input gather the selected rows of sparse
batches into batches of 64, and the index lookup's model counts the
batches gathered.

- **Machine**: Apple M5 Pro, macOS 26.6.2, on AC power. Another
  session's benchmarks used the machine at 19:27–19:31 and from 20:22;
  the TPC-H runs below ran from 19:31 to 20:15, the families' from
  20:55, the load average 1.2 to 2.
- **PostgreSQL** master (20devel), a release build without assertions.
- **Base**: `9c8b2953`. **Change**: `37cfba07`, its code; `b04868b1`,
  the same code with the join family's sparse case corrected, for
  `ab-join-gather.md`, `ab-join-again.md` and `ab-win.md`.

## TPC-H

`cargo tpch compare`, SF 1, base and change in turn, three pairs a
query. A ratio is the change's time with Tessera on over the base's;
below one is faster; the control is the same with Tessera off.

- Serial ([`tpch-serial-pair1.txt`](tpch-serial-pair1.txt),
  [`tpch-serial-pair2.txt`](tpch-serial-pair2.txt), runs
  `tpch-sf1-ABmVvX` against `tpch-sf1-Jq4cU4`, `tpch-sf1-obiRto`
  against `tpch-sf1-ku5aY4`): the geometric mean 0.838 and 0.825, the
  controls 1.004 and 0.982; without Q09 0.929 and 0.918. Q14 0.78 and
  0.80, Q16 0.76 and 0.83, Q11 0.82 and 0.83, Q15 0.86 and 0.87, Q03,
  Q05, Q08, Q10 and Q21 0.85 to 0.96. Q09 0.12 and 0.11: the lookup's
  model turned its plan, 6.2 times the core's time on the base, to
  0.73 of it. No query slower by 4 % in both pairs: Q04 1.02 and 0.95,
  Q12 0.99 and 1.04, Q13 1.04 and 0.98.
- Two workers ([`tpch-w2-pair1.txt`](tpch-w2-pair1.txt),
  [`tpch-w2-pair2.txt`](tpch-w2-pair2.txt), runs `tpch-sf1-XZiIzu`
  against `tpch-sf1-bluMsx`, `tpch-sf1-shHKAb` against
  `tpch-sf1-5fQwDI`): 0.861 and 0.888, the controls 1.003 and 1.038,
  the second pair begun at a load of 4.3; without Q09 0.954 and 0.981.
  Q03 0.80 and 0.79, Q14 0.87 and 0.88, Q10 0.89, Q11 0.91. Q04 1.11
  and Q16 1.08 in the second pair only, against 1.00 and 1.01 in the
  first.

## Q14, node by node

`EXPLAIN (ANALYZE, VERBOSE)` of Q14 on the TPC-H cluster, serial, the
best of three runs; the base's from the measurement that sized the
change, on the same base. A node's own time is its time less its
children's.

- The aggregate: 41 470 batches in, 23.7 ms of its own; with the
  change 1 190 batches, 4.1 to 4.3 ms.
- The join: 21.6 ms of its own; with the change 13.4 to 14.4 ms, the
  copy of 75 980 of its 75 983 outer rows included.
- The query under `EXPLAIN`: 194 ms; with the change 125 to 134.

## The families

`cargo ab measure` against the base, two rounds a side, seven
repetitions, the cases in groups so that a run stays within five
minutes. A case is flagged when it moves past 2 % by its least time
and by its median beyond the control.

- `join`, the gathering cases
  ([`ab-join-gather.md`](ab-join-gather.md)): a fact table's probe side through a filter that keeps a row in
  twenty, 0.85; a numeric summed above a filter, 0.97; two text columns
  of 6 111 rows copied before a join that pairs 1 000 of them, the
  copy's worst case, 0.99; a column a subquery computes, not gathered,
  1.01.
- `join`, the other cases ([`ab-join-a.md`](ab-join-a.md),
  [`ab-join-b.md`](ab-join-b.md), [`ab-join-c.md`](ab-join-c.md)): five
  flagged slower by 2 to 4 %, `dup`, `fk_count`, `fk_outer_col`, `int8`
  and `left_nulls`, all over dense batches, which the input hands on as
  they come. Again alone with 51 repetitions
  ([`ab-join-again.md`](ab-join-again.md)): none flagged, the medians
  within 1.4 % of the controls, the least times 1.8 to 4.0 % above the
  base's.
- `indexjoin` ([`ab-indexjoin-a.md`](ab-indexjoin-a.md),
  [`ab-indexjoin-b.md`](ab-indexjoin-b.md)), `tax`
  ([`ab-tax.md`](ab-tax.md)): none flagged.
- `win`, its seven cases of aggregates above filters
  ([`ab-win.md`](ab-win.md)): none slower, `sum_sparse` 0.97.

The first runs of the families and of `lookupcost` that day measured
another worktree's cluster: `bench/pg/run.sh` restarts its cluster on
the port of its last start, 55471 here, and sends its queries to
`PGPORT`, 5433 by default, where that cluster listened. The runs above
set `PGPORT` and checked the server's `data_directory` first.

## The lookup's calibration

`bench/pg/run.sh measure lookupcost` on the change
([`lookupcost-summary.txt`](lookupcost-summary.txt),
`pg-lookupcost-HDCBu5`; `HI69LR` and `h3HzqF` alike), with samples
whose pages keep 22 to 33 rows of 64 to a filter. Fitted together, the
seven prices came out as a lookup's batch of 23 and a key below
nothing; fitted alone, the copy at 0.31, 60 ns a value, where an
integer's copy takes a few. The calibration takes a lookup's time as
the query's less its outer side's alone, whose aggregate now gathers
the same rows, so the copies are not apart in it, and
`tessera.join_lookup_copy_cost` keeps its first estimate, 0.015. With
the prices fitted, the model chose as the times did in 18 samples of
19. The planner counted the batches of the shapes of 22 to 33 rows at
1 070 to 1 732 where 1 590 to 3 216 came: a filter on `id % 100` keeps
rows in a pattern of the table's order, not at random.

## Simulations

- [`simulate-rules.py`](simulate-rules.py): the rules over a heap
  scan's pages of 40 to 140 rows, cut into batches of 64 and a short
  rest, under filters that keep 2 to 90 % of the rows, a batch counted
  at 1 µs and a copied row at 12 ns. Against no gathering, the worst:
  the approved rule 1.13, on pages of 83 to 110 rows at 55 to 90 %; the
  rule kept, a sparse batch right after one that went as it is going
  as it is too, 1.02; copying full batches that fit, 1.10.
- [`simulate-count.py`](simulate-count.py): the planner's count of
  gathered batches against the rule kept, batch by batch over binomial
  batches of 34 to 64 rows: within 1.8 %.
