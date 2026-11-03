# Index lookups by batch in the hash join, runs of 2026-11-03

The runs that the change `batch-index-join` cites: TessHashJoin looks
the keys of each outer batch up in a btree index of the inner side, in
one scan with the keys as an array, and builds a table of the rows
found for that batch, where its cost model finds it faster than the
core's nested loop over the index.

- **Machine**: Apple M5 Pro, macOS 26.6.2; other sessions of work shared
  it, so the times come as pairs, each with the core's times as its
  control. One base run of TPC-H ran on battery
  ([`tpch-serial-pair1.txt`](tpch-serial-pair1.txt), its header).
- **PostgreSQL** master (20devel), a release build without assertions.
- **Base**: `b6ddc7ea`, the benchmark family alone. **Change**: the
  first model of the lookup's cost, `ad081e4a`; the fitted model,
  `eae37523` for TPC-H and `d4eac1d7`, the final one, for the family:
  the final commit differs from `eae37523` in the cost of the lookups
  only, and takes no lookup in any TPC-H query at scale 1, serial or
  with two workers (each query explained), as `eae37523` took none.
  These are the change's commits before they moved onto `main`.

## The family indexjoin

`cargo ab measure`, `bench/pg/indexjoin.sql` at scale 1, two runs a
side alternating. A ratio is the change's time over the base's; below
one is faster.

- The first model ([`ab-indexjoin-first-model.md`](ab-indexjoin-first-model.md),
  runs `pg-indexjoin-G6QBhn`, `pg-indexjoin-Nn41S9` against
  `pg-indexjoin-7uuqsz`, `pg-indexjoin-zXbVCN`): keys in the index's
  order 0.63 (`pk`), 0.27 (`pk_20k`), 0.62 to 0.79 for the others; but
  keys shuffled 1.12, one key a batch 1.27, a text key 1.08, each where
  the lookup took the core's nested loop's place.
- The final model ([`ab-indexjoin.md`](ab-indexjoin.md), runs
  `pg-indexjoin-7EsYXT`, `pg-indexjoin-K4TnhP` against
  `pg-indexjoin-clKC60`, `pg-indexjoin-t56MmG`): `pk` 0.62, `pk_20k`
  0.25, `cross_type` 0.61, `date_key` 0.62, `scattered` 0.69, `four`
  0.80 by the least run; `pk_shuffled`, `sparse`, `text_key` and
  `left_miss` keep the core's nested loop. The load reached 13 during
  this run and the controls moved 0.81 to 0.97: the medians of the cases
  whose plan did not change (`pk_shuffled` 0.70, `sparse` 0.69) are the
  machine's drift. A run of `eae37523` under a quiet machine
  (`pg-indexjoin-cfHZ5H`, `pg-indexjoin-5tRd7L` against
  `pg-indexjoin-pfIeoG`, `pg-indexjoin-VNRzz4`) gave the same plans and
  0.28 to 0.81 for the lookups.

## The calibration

`bench/pg/lookupcost.sql`, two runs ([`lookupcost-1.txt`](lookupcost-1.txt),
run `pg-lookupcost-zQqYRs`, and [`lookupcost-2.txt`](lookupcost-2.txt),
`pg-lookupcost-94qad9`): the defaults are their mean. The planner's
counts against the plan's: batches 103 of 103 where the keys come in
the table's order, descents 237 against 238; the descents of keys a
stride apart or alternating with keys past the index's end are an
upper bound. From its own counts and within the margin of 0.8, the
planner took the lookup in 12 of the 26 samples, each where it ran
faster, and left four lookups that ran 3 to 21 % faster to the nested
loop. Runs of the models tried before, for the history: shares of the
core's estimate of a scan for one key gave negative shares
(`pg-lookupcost-KdJuxo`, `pg-lookupcost-O1plzo`); fitted times with a
price of their own for a row found a row nearly free
(`pg-lookupcost-eUgujk`, `pg-lookupcost-pzhUKQ`), and TPC-H Q21 took a
lookup of 2.9 million rows, 2.2 times slower (`tpch-sf1-1StXUV`
against `tpch-sf1-Gyoefl`); the rows priced by the scan model, the
whole cost still at the hash join's rate (`pg-lookupcost-KpX242`,
`pg-lookupcost-xQxH7g`), took it again, until the inner side's work
went into the cost as a scan's.

## TPC-H

`cargo tpch compare`, SF 1, base and change in turn, three pairs a
query.

- The first model ([`tpch-first-model.txt`](tpch-first-model.txt), run
  `tpch-sf1-nGKzZa` against `tpch-sf1-mVx1yP`): Q09 0.097 of the base's
  time (4.9 s to 0.48 s, a lookup into `orders` on a join the core
  expects 98 rows of where 319 404 come), Q02 0.85; Q05 1.27 (46 000
  outer rows in 21 700 batches, about two keys a lookup), Q16 1.19 (a
  lookup of 118 000 rows in place of a hash join over `partsupp`). A
  second pair is lost to a build of another session (controls 1.15).
- The fitted model, serial ([`tpch-serial-pair1.txt`](tpch-serial-pair1.txt),
  [`tpch-serial-pair2.txt`](tpch-serial-pair2.txt), runs
  `tpch-sf1-1ZMtPq` against `tpch-sf1-lYhxer`, `tpch-sf1-JpS0At`
  against `tpch-sf1-UCETbC`): the geometric mean 1.017 and 0.969, the
  controls 1.042 and 0.977; no query took a lookup. With two workers
  ([`tpch-w2-pair1.txt`](tpch-w2-pair1.txt),
  [`tpch-w2-pair2.txt`](tpch-w2-pair2.txt), runs `tpch-sf1-Ezcmkq`
  against `tpch-sf1-hQ6UFO`, `tpch-sf1-SsuHzs` against
  `tpch-sf1-virdBq`): 0.995 with the control 0.999; the second pair
  moved 1.17 with its control 1.16, a load of 9 from another session.
- A first try of four runs measured the base four times: the bench
  worktree held a copy of a file the change adds, and its checkout of
  the change failed. Its pairs, base against base, give the drift:
  1.001 and 1.034, single queries within 5 to 10 %.
