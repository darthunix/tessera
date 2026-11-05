# Semi and anti joins by index lookups, runs of 2026-11-05

The runs that the change `semi-anti-lookups` cites: TessHashJoin's
index lookup mode offered for SEMI and ANTI joins, its cost model
counting the core's nested loop's rows to the first match.

- **Machine**: Apple M5 Pro, macOS 26.6.2, on AC power. A system
  process of macOS (`photoanalysisd`) took about one core through the
  runs; the load average was 1.3 to 6.
- **PostgreSQL** master (20devel), a release build without assertions.
- **Base**: `481911ee`. **Change**: `5220f509`, its code; the commit
  after it changes the list of the plans `indexjoin` writes.
- **Data**: TPC-H at SF 1; the bench cluster at the data's scale of 1,
  with the table `bench_four` the change adds to the data set.

## TPC-H

`cargo tpch compare`, base and change in turn, three pairs a query. A
ratio is the change's time with Tessera on over the base's; below one
is faster; the control is the same with Tessera off.

- Serial ([`tpch-serial-pair1.txt`](tpch-serial-pair1.txt),
  [`tpch-serial-pair2.txt`](tpch-serial-pair2.txt), runs
  `tpch-sf1-TsLQzA` against `tpch-sf1-CcjHNU`, `tpch-sf1-FQwsSV`
  against `tpch-sf1-XF3D7l`): the geometric mean 0.928 and 1.061, the
  controls 0.927 and 1.054. Two workers
  ([`tpch-w2-pair1.txt`](tpch-w2-pair1.txt),
  [`tpch-w2-pair2.txt`](tpch-w2-pair2.txt), runs `tpch-sf1-fMNcA6`
  against `tpch-sf1-FIMOuS`, `tpch-sf1-9heuBX` against
  `tpch-sf1-6xyy4K`): 1.034 and 0.997, the controls 1.030 and 0.999.
- The plans: the nodes of every query the base's, in all four pairs.
  The model takes no semi or anti lookup in Q04 or Q21; the lookups of
  Q05, Q09, Q10 and Q12 are inner joins', as on the base.
- Q15, which has no semi or anti join and the base's plan, came slower
  beyond its control in both serial pairs, by 7 % and 5 %. Measured
  again alone, seven pairs, base and change in turn: 1.031 and 1.063,
  the controls 0.991 and 1.003
  ([`tpch-q15-pair1.txt`](tpch-q15-pair1.txt),
  [`tpch-q15-pair2.txt`](tpch-q15-pair2.txt)); both sides built with
  `COPT=-falign-functions=64`: 1.014 and 1.025, the controls 0.989 and
  1.015 ([`tpch-q15-aligned-pair1.txt`](tpch-q15-aligned-pair1.txt),
  [`tpch-q15-aligned-pair2.txt`](tpch-q15-aligned-pair2.txt)). The
  code Q15 runs is the base's: the change touches the lookup's planner
  and a check at the start of a join in lookup mode.

## Q04 and Q21 with the lookup forced

Q04 and Q21, five pairs, the change's own plans against the same
build with the lookup's batch, descent and key prices set to nothing
and the nested loop's key priced past the margin, in the database's
settings for the run.

- [`tpch-q04-q21-lookup-forced.txt`](tpch-q04-q21-lookup-forced.txt)
  (`tpch-sf1-8wNZIf` against `tpch-sf1-AFwpcx`): Q04 1.057, the control
  0.998, its semi join a lookup: 896 lookups of 57 218 keys, 12 654
  descents, 229 690 rows of `lineitem` read, four a key, where the core's
  loop reads about two. Q21 0.973, the control 0.989: the lookup went to
  its join with `orders`, which the planner moved before the anti join;
  the anti and semi joins stayed the core's nested loops.
- [`tpch-q04-q21-nested-loop-off.txt`](tpch-q04-q21-nested-loop-off.txt)
  (`tpch-sf1-8wNZIf` against `tpch-sf1-EezlHl`), nested loops off too:
  Q04 0.941 with the same plan, so that Q04's lookup is even with the
  core's loop within the machine's noise. Q21 1.126: every join a
  lookup, the join with `orders` first, then the semi join over 75 871
  keys and the anti join over 73 089, reading all of each order's lines,
  about 710 000 rows of `lineitem` where the core's plan reads about
  330 000, its anti join first cutting 156 739 rows to 13 859.
- The anti join alone in its own place ([`q21-fenced.sql`](q21-fenced.sql),
  [`q21-pgbench.txt`](q21-pgbench.txt)): Q21 with `l1`, `supplier`,
  `nation` and the anti join under `OFFSET 0`, which keeps the anti join
  first, timed by `pgbench` for 90 seconds against Q21 itself and the
  same query with the anti join forced to a lookup (the prices as above,
  nested loops off), the three scripts drawn at random in turn. The
  means: Q21 174.9 ms (149 runs), the fenced query 178.3 ms (179), with
  the lookup 186.6 ms (172), 8.3 ms more with a standard error of about
  1.8. The anti join's lookup: 2600 lookups of 156 739 keys, 12 744
  descents, 519 545 rows built, 199 787 pairs checked by `l3.l_suppkey
  <> l1.l_suppkey`, where the core's loop reads about 1.9 rows a key.
  Nested loops off sent the outer joins, over 13 859 and 6923 keys, to
  lookups too. The three answered the same.

## The families

`cargo ab measure`, base against change.

- `indexjoin` ([`ab-indexjoin.md`](ab-indexjoin.md)): the semi and
  anti cases over `bench_four` that the model takes to a lookup:
  `semi_fifth` 0.21, `anti_fifth` 0.30, `anti_both_sides` 0.30,
  `anti_none` 0.37, `semi_three_fifths` 0.67; against the core's nested
  loop in `lookupcost`, 0.34 to 0.61. `semi_shuffled` was flagged at
  1.058 against a control of 0.949, with the core's loop on both sides;
  measured again alone with 31 repetitions in three rounds, 0.997 and
  0.976 ([`ab-indexjoin-semi_shuffled.md`](ab-indexjoin-semi_shuffled.md)).
  `semi_pk`, 1.014 and 1.029, measured again alone, 0.994 and 1.038
  against a control of 1.000
  ([`ab-indexjoin-semi_pk.md`](ab-indexjoin-semi_pk.md)): the core
  makes a semi join by a unique key an inner join, the same lookup on
  both sides. `semi_all`, `anti_miss` and `semi_shuffled` keep the
  core's loop: `semi_all`'s lookup ran in 0.58 of the loop's time in
  `lookupcost`, where the model counts 0.84, past the margin of 0.8.
- `join` and `exec` passed the five minutes; the plans of their runs
  that ended (`pg-join-7SVOrp` against `pg-join-rqyjNR`, `pg-exec-CDMeIC`
  against `pg-exec-PtG68H`) are the same line for line, no lookup in
  either, so they were not timed further.

## The model against the times

`bench/pg/lookupcost` twice on the change
([`lookupcost-1.txt`](lookupcost-1.txt), run `pg-lookupcost-DzhUpz`;
[`lookupcost-2.txt`](lookupcost-2.txt), `pg-lookupcost-Loy1vl`), each
lookup forced past the margin for its run. Of the 14 semi and anti
samples, which are not fitted, the model chose as the times did in all
of them in both runs: no lookup taken that ran slower, none left to the
loop that ran faster. It counts the lookup's time 4 to 53 % high and
the loop's 8 to 75 % high. The planner's rows to the first match came
within 4 % of the query's count, but where a clause over both sides
passes about half the pairs (its estimate a third): 11 645 against
10 259, and with a second key 18 181 against 16 070. The prices fitted
over all the inner samples together came out as a batch of 30.5 and
25.2 and a key below nothing, as in the runs of the change before.
