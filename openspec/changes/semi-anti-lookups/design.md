## Context

A semi join returns the outer rows that have a pair, once each; an anti
join returns those that have none. PostgreSQL plans `EXISTS` and
`NOT EXISTS` subqueries as such joins. The core's nested loop over an
index runs one index scan an outer row and stops at the first inner row
that passes the inner relation's clauses and the join's other clauses.

TessHashJoin's lookup mode (`docs/nodes.md`, Index lookups) takes the
keys of an outer batch, up to 64, looks them up in one scan of an index
of the inner table with the keys as an array, builds the batch's table
from the rows found and probes it. Keys near each other in the index
share the descents of the tree. The mode's execution does not depend on
the join type: the batch's table is built before the probe, the semi
and anti probe walks a row's candidates until the join's other clauses
pass, keeps the outer rows as many as they come, and returns a row with
no pair for an anti join; an outer row whose key is NULL is looked up
with no key. Two gates stop semi and anti lookups today: the planner
offers the mode for INNER and LEFT joins only, and `join_begin` refuses
any other type in a plan, as foreign plan data.

In TPC-H at SF 1:

- Q4's semi join reads, for 57 218 orders of a quarter, the lines of
  each order by `lineitem_pkey` until one has `l_commitdate <
  l_receiptdate`: 0.92 rows found and about one removed an order of the
  four it holds; 57 ms of the query serial. The orders' keys come in
  `lineitem`'s order, about one order in 26.
- Q21's anti join reads, for 156 739 lines, the lines of the same order
  until one passes `l3.l_receiptdate > l3.l_commitdate AND l3.l_suppkey
  <> l1.l_suppkey`: 84 ms of 170; its semi join does the same for 13 859
  lines. The keys come in `lineitem`'s order, about one outer row a
  key.

A lookup reads every row of a key, about twice the rows the nested loop
reads in both queries, and spares the nested loop's index scan an outer
row: one scan a batch, and the descents keys near each other share.
Which of the two weighs more is not known before the measurement.

The maintainer took the entry out of turn on 2026-11-03, put it after
the gathering of sparse batches on 2026-11-04, and on 2026-11-05 put
into it the repair of `bench/pg/run.sh`'s port and a new measurement of
the families of `deform-past-varlena`, whose A/B runs had measured
another worktree's cluster.

## Goals / Non-Goals

**Goals:**

- Semi and anti joins by index lookups where the model says they are
  faster than the core's nested loop, with the same rows.
- A model that counts apart what each side does: the rows each reads,
  the rows the table takes, the candidate pairs checked, each to the
  first match where the join stops there.
- A bench harness that measures the cluster it names.

**Non-Goals:**

- A scan that stops at a key's first passing row ("Refused" below); it
  becomes a roadmap entry.
- The count of a batch's distinct keys. The btree reads a key repeated
  in a batch once, but how often keys repeat in a batch is known only
  where the path keeps the outer key's order, which a filter or a join
  below the lookup can break. The model counts every key, which errs
  against the lookup, as it does today. Q4's keys are the orders' own
  and do not repeat; Q21's repeat where two late lines of one order
  come from suppliers of the nation.
- The prices of INNER and LEFT lookups, which stay as they are.
- RIGHT SEMI and RIGHT ANTI joins by lookups: the inner side's rows are
  what they return, and a lookup reads only those its outer keys reach.

## Decisions

### Lookups for semi and anti joins

**Was:** `lookup_found` is computed for `JOIN_INNER` and `JOIN_LEFT`,
and `join_begin` refuses a lookup of any other type.

**Will be:** both take `JOIN_SEMI` and `JOIN_ANTI` too. A parallel path
divides the outer side as for the other lookups; each participant looks
its batches up alone. A lookup's table does not spill: past its memory
it grows and shows `Overrun`, as for the other lookups.

**Refused:** the core's way for a semi join, the inner side made unique
and joined as an inner join: the semi probe already returns a row once.

### The model

The model of a lookup (`docs/costs.md`, the index join) prices the
lookup and the core's nested loop over the same keys and offers the
lookup within `JOIN_LOOKUP_MARGIN`, 0.8, of the loop.

**Was:** one count of rows, `keys × m` (m the rows a key holds by the
equality alone), served both sides: the rows the lookup's scan read,
the rows its table took, the rows the nested loop read, and, with the
inner clauses' share, the candidate pairs the join's other clauses
checked. A batch went out in as many rounds as a key has rows.

**Will be:** the counts are kept apart. With `keys` the outer rows that
have a key, `a` the share of the inner rows the inner relation's
clauses keep, and `s` the share of a key's m rows that pass those
clauses and every other condition of the join:

```
                      INNER, LEFT       SEMI, ANTI
lookup's scan reads   keys × m          keys × m
its table takes       keys × m × a      keys × m × a
nested loop reads     keys × m          keys × E(m, s)
loop's pairs checked  keys × m × a      keys × E(m, s) × a
probe's pairs checked keys × m × a      keys × E(m, s) × a,
                                        an upper bound
rounds a batch        rows a key        1, or m × a with
                                        other clauses

E(n, p) = (1 - (1 - p)^n) / p,  E(n, 0) = n
```

`E(n, p)` is the rows read until the first of `n` that passes when each
passes with the chance `p` alone; an outer row with no match reads all
`n`. The loop's pairs checked are the rows that passed the inner
clauses, as the loop meets them, each checked by the join's other
clauses. The probe checks fewer: its table takes no row with a NULL in
a key, and it hands on to the other clauses only the candidates that
match every key of the join, where m counts the rows of the one key the
index looks up. The model counts the probe's checks as the loop's, an
upper bound, against the lookup. The semi and anti probe publishes a
batch once: a round takes each row's next candidate, and rounds go on
while the join's other clauses leave a row without a pair and a
candidate is left, one round where there are no such clauses.

`s` comes from the core's estimate of the join, which it computes for
every semi and anti join (`compute_semi_anti_join_factors`):
`outer_match_frac × match_count` is the pairs an outer row finds, the
join's clauses at an inner join's selectivity over the inner rows that
pass their own clauses; for an anti join the clauses applied after the
join are not among them. So `s = min(1, outer_match_frac × match_count
/ m)`. The planner passes `JoinPathExtraData`'s factors to the model.

This is an approximation in three ways, written in `docs/costs.md`: m
is an average, the rows of a key pass alone of each other, and where the
core raises `match_count` to one, `s` comes out high, so that the
nested loop is counted cheaper, against the lookup.

`Lookup Estimate` in EXPLAIN gains `loop_rows=`, the nested loop's
rows, and `checks=`, the probe's pairs checked.

**Refused:**

- The core's own share for its nested loop's semi join, `2 /
  (match_count + 1)` of the inner scan for an outer row that matches: a
  rule of its cost, not the expected count. For four rows a key, each
  passing with a half, it gives 2.6 rows to such a row, where the
  expectation is 1.7.
- A price of its own for the rows the nested loop reads to the first
  match: they cost what its other rows cost.

### Calibration

**Was:** `lookupcost` fits the lookup's and the loop's prices over
samples of inner joins. It calls the rows built "fetched", counts the
keys given with their repeats, and takes the nested loop's rows from the
lookup's plan.

**Will be:** the samples keep apart the rows the lookup's scan read (its
filter's input rows) and the rows built. Semi and anti samples are
added, with and without a clause over both sides, inner clauses that
pass none, a fifth, three fifths and all of a key's rows, keys in order
and shuffled. They are not fitted: the prices stay fitted over the inner
samples, and the summary prints each semi and anti sample against the
model's prediction and the choice it makes. A sample of two keys,
one of them looked up, checks the probe's bound.

The nested loop's rows of a semi or anti sample, which no plan shows
exactly (EXPLAIN rounds the rows a loop removes), come from a query
that walks each key's rows of the sample's tables in the index's order:
by every column of the index, then by the row's address, the order of a
btree's equal keys. The samples' tables are never updated, so a row's
address is the one its index entry holds. The probe's pairs checked
come from the join's `Matches`, which counts each round's candidates,
those its clauses reject too; the probe walks a key's candidates in its
table's order, not the index's, so no query over the tables gives
them.

A sample's lookup is forced for its run: the lookup's batch, descent
and key prices set to nothing, and the loop's key price
(`tessera.join_loop_key_cost`) raised until the lookup passes the
margin, the plan checked to look up. Where the lookup reads many rows a
key and the loop one, its scan and build alone can cost more than the
loop. The model's own choice is computed from the plan's estimate at
the prices as set, apart from the forced run.

### The bench cluster's port

**Was:** `bench/pg/run.sh` restarts its cluster with the options of its
last start (`postmaster.opts`), whatever port they name, and sends its
queries to `PGPORT`, 5433 by default.

**Will be:** every start and restart (`setup`, `restart`, `measure`)
names the port and the socket directory. Before a family runs, on both
of `measure`'s paths, the one that restarts and the one whose caller
did (`BENCH_RESTARTED`), it checks that the server it reaches reports
the cluster's data directory, and stops otherwise. The families `tax`
and `win` of `deform-past-varlena` are measured again against its base,
and the runs replace the ones its benchmarks cite.

### Refused: a scan that stops at a key's first row

For a semi or anti join that depends on the key alone, the scan could
return a key's first row that passes the inner clauses and skip the
key's other index entries unread, reading no more rows than the nested
loop. It is left out:

- The supported core no longer offers what it needs. PostgreSQL master
  has neither `index_getnext_tid` nor `index_fetch_heap`: the table
  access method returns rows (`table_index_getnext_slot`), and its call
  for a TID alone (`tableam_index_getnext_tid`) leaves the row's fetch
  private to the heap. Asking for index tuples (`xs_want_itup`) makes
  the scan index-only, with a virtual slot. The scan would own the
  heap's visibility, HOT chains, pins and the killed-tuple protocol.
- It holds only for one join key and no other condition of the join at
  all, but the key's own recheck: an anti join made of a LEFT join can
  keep a volatile clause over no column, such as `nextval('s') % 2 =
  0`, which a key's first row fails and its second passes.
- It needs an owned copy of the key answered last, reset at each lookup
  and rescan, its own costing of clauses evaluated row by row, and its
  counters carried through the join for workers.

The roadmap entry records these, to be taken if the measurement shows
Q4 needs it.

## Risks / Trade-offs

- A lookup reads every row of a key; where the model errs on `s` or m,
  it may take a lookup slower than the loop. The margin of 0.8 and the
  calibration's semi and anti samples bound it.
- The core estimates a clause over two columns of one table, such as
  `l_commitdate < l_receiptdate`, at a third, where TPC-H passes about
  three fifths: the nested loop's rows come out high for Q4, in the
  lookup's favor.
- A semi or anti join over an outer side whose keys repeat is priced as
  if they did not: the lookup is counted dearer than it is.
- The model prices the core's loop over the lookup's index and its one
  key. Where the core's loop uses an index of more of the join's keys,
  it reads fewer rows than `E(m, s)` counts, and the model favors the
  lookup; the margin and the sample of two keys bound it.

## Measurement plan

The tools of `docs/measuring.md`, on Apple M5 Pro on AC power, an idle
machine, five minutes a run at most, the families on the bench cluster
whose server the run checks.

- TPC-H at SF 1, `cargo tpch`, base and change in turn, two pairs,
  serial and with two workers; Q04's and Q21's plans and counters; Q04
  and Q21 also with the lookup forced as `lookupcost`'s samples are,
  the plans checked to look up, so that a gain the model leaves
  shows.
- `cargo ab measure`: `indexjoin` with new semi and anti cases (with and
  without a clause over both sides, inner clauses passing none, a
  fifth, three fifths and all rows, keys in order and shuffled); `join`
  and `exec` for the semi and anti joins of the hash mode.
- `lookupcost` twice.

The thresholds that decide:

- the geometric mean of TPC-H, serial and with two workers, no worse
  than the base's beyond the control, and no query slower by 5 % beyond
  the control in both pairs;
- Q04 or Q21 at most 0.9 of the base's time, serial; if neither
  improves by a tenth, the maintainer decides whether the change stays;
- no case of the families flagged slower at 2 % once measured again
  alone;
- in `lookupcost`, among the semi and anti samples, no lookup taken
  that runs slower than the nested loop; a lookup that runs faster and
  that the margin leaves to the loop is reported, and does not fail;
  the inner samples' choices as before.

## Files

- `nodes/join_planner.c`: the join types, the counts, the factors.
- `nodes/hashjoin_begin.c`: the join types at the start.
- `bench/pg/run.sh`, `bench/pg/indexjoin.sql`,
  `bench/pg/lookupcost.sql`.
- `docs/nodes.md`, `docs/costs.md`, `openspec/roadmap.md`.

## Tests

- `test/sql/index_join.sql`: semi and anti joins by lookups, each plan
  asserted to look up, against the core's rows: NULL outer keys, keys
  repeated within a batch and absent from the index, inner clauses that
  pass no row, some or all of a key's rows, a clause over both sides, a
  hashed key whose equality the join checks again, a table past its
  memory with a clause over both sides, workers, a correlated subquery
  rescanned with a changed parameter; `Lookup Estimate`'s `loop_rows`
  and `checks` beside the counts of the core's plan for a key of four
  rows of which one, two and none pass.
