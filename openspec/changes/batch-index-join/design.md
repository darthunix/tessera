## Context

The core joins a small outer side to a large indexed table with a
nested loop: for each outer row it rescans an index scan with the
row's key, which descends the btree from its root, fetches the heap
tuples found, and evaluates the scan's other clauses row by row. With
Tessera on, TPC-H keeps these loops at SF 1: Q5 and Q7 look up
`lineitem_pkey` by `l_orderkey` (about 4 rows a key before the scan's
filter), Q8 `orders_pkey` and `customer_pkey`, Q21 `orders_pkey`, and
`lineitem_pkey` for `EXISTS` and `NOT EXISTS`, as Q4 does; with two
workers Q3 and Q8 do it too.

What was measured, serial, from runs `tpch-sf1-867yIt` (serial) and
`tpch-sf1-HER69p` (two workers) and plans under EXPLAIN ANALYZE, whose
timing inflates nodes called per row:

- A nested loop's time less its outer child's: Q4 about 87 ms for
  57 000 lookups, Q5 88 ms for 46 000, Q7 158 ms for 121 000, Q8 56 ms
  for 57 000 in two loops, Q21 115 ms for 179 000 in three; about
  500 ms in all.
- With nested loops off, the node's hash join over full scans wins a
  little in Q7 and Q8 and loses in Q4 (3 times as long) and Q21 (2.3).
- A profile (macOS `sample`, of the query's samples): the btree's
  descent (`_bt_first`: `_bt_search`, `_bt_compare`, the buffer lookup
  of each level) 35 to 38 %, the heap fetch and its slot 10 to 15 %,
  the scan's clauses row by row 6 to 20 %, the index scan's rescan 1
  to 2 %.
- The keys of 125 000 orders (every 12th) against `lineitem_pkey`, the
  outer side in key order: one lookup a row, 125 000 descents, 488 000
  buffer hits, about 172 ms; arrays of 64 keys, one scan each, 1 954
  scans, 13 068 descents, 156 000 hits, about 133 ms; one array, 6
  descents, 132 000 hits, about 126 ms.

The arrays save about 20 µs a batch of 64 keys in that case: the
fixed work a batch adds must stay well below it. Over Q5, Q7, Q8 and
Q21 the change aims at 100 to 150 ms serial at SF 1; the measurement
of the join itself, not of the array scan alone, decides.

## Goals / Non-Goals

**Goals:** INNER and LEFT joins the core plans as index nested loops,
done by TessHashJoin over batches of the outer side: one index scan a
batch for its keys, the rows found put into a heap batch and the
inner side's clauses run in batches, the core's index access methods
used as they are, the rows of the core's plan. Any key type the join
takes. The change is accepted by its measurement, which the maintainer
weighs: the eligible lookups of Q5, Q7, Q8 and Q21 serial and of Q3
and Q8 with two workers, and the benchmark family.

**Non-Goals, each for later or never:**

- SEMI and ANTI joins: the core's nested loop stops at the first match,
  and the mode reads every row of a key; Q4 and Q21 read about 4 rows
  a key where the core reads 1.6. A roadmap entry.
- A batch whose table outgrows `hash_mem`: here the planner keeps such
  joins out and the node counts the excess; splitting a batch's keys
  into lookups that fit is a roadmap entry.
- RIGHT, FULL and RIGHT ANTI joins, which need the whole inner side.
- A btree of the node's own; keys that are not the first column of an
  index or that are an expression.

## Decisions

### The lookup mode of TessHeapScan

The plan of a lookup scan names its relation, the index, and the
lookup's scan key: index attribute 1, `BTEqualStrategyNumber`, the
operator's function, its input collation and its right-hand type as
the subtype, as the core's index scan builds an array key
(`ExecIndexBuildScanKeys`). At its start the node opens the index with
the relation's lock mode, unless the plan is only explained, and
begins an index scan of that key (`index_beginscan`, the heap's
snapshot `es_snapshot`). The join gives it an array of keys of the
outer side's type, as SQL values (a date stays a date, a text a text);
the node puts the array into the key as an array key
(`SK_SEARCHARRAY`), rescans the index (`index_rescan`), and fills its
batches from the index fetch (`table_index_getnext_slot`), each row the
tuple where it lies on its pinned page (`tess_heap_batch_append_slot`),
as its index mode does with the core's index scan. A btree sorts the
array, drops its duplicates and NULLs at each rescan and walks its
leaves from key to key, so the join neither sorts nor deduplicates.
The visibility of a row, HOT chains and killed entries are the index
fetch's. At its end the node ends the scan and closes the index.

Every place where TessHeapScan chooses between its modes takes the
lookup mode: its start begins no heap scan, it sets up no parallel
heap scan, a rescan rescans no heap scan, it prints no pages, and its
batches come from the lookup. A rescan forgets the keys: until the
next call the key reads as NULL and finds nothing. So the join first
rescans its inner child, which TessFilter passes down to the scan,
and then makes the call, which puts the array into the key and
rescans the index.

The array lives in a context of the join that is reset only before
the next batch's call: the btree reads the array when the scan starts
fetching and keeps pointers into it while it runs.

Refused: a parameter of the core's plan that the join sets for an
index scan of `key = ANY($p)`. The core accepts a parameter only from
its own nodes: `finalize_plan` rejects one a CustomScan sets ("plan
should not reference subplan's variable"), EXPLAIN cannot name its
owner and asserts on it in a build with assertions, and a way around
(a placeholder replaced after planning, the dependencies written by
hand through the subtree) is fragile. Refused: a callback of the C API
for the keys; both nodes are of one module, and nothing outside it
needs the call yet.

### The lookup path belongs to its join

The lookup scan's path is made only inside the join's path and never
enters the inner relation's lists of paths: no other plan may take a
scan that nobody gives keys. Its eligibility is the heap scan's own
(`relation_supported`, `plain_heap_relation`: no inheritance parent, no
lateral reference, no row marks, no sample, the heap access method),
with no pseudoconstant restriction of the relation, which would put
the core's gating `Result` above the scan, and the relation's
restrictions go to TessFilter above it by the
scan planner's own helpers, all of them, in their security order. At
its start the join finds the lookup scan, its inner child or under one
TessFilter, checks that it is in the lookup mode, and keeps it; the
call is a function of `nodes/heapscan.c` for the module. The join
rescans its whole inner child before the call.

### A table per outer batch in TessHashJoin

In this mode the join builds no table at its start and never treats an
empty table as the end. It asks its outer child for batches of at most
64 rows, or fewer when its parent asks for fewer; a larger batch is an
error, not an overrun. One step, before an outer batch is probed, in
`join_outer_next` (which the rounds, the matches and the row output all
read through), prepares the batch:

1. the keys of its rows with a key, as an array of the outer key's
   type, in the batch's order;
2. a batch without one gets an empty table and finds no pairs, so LEFT
   returns its rows with NULL inner columns;
3. otherwise a rescan of the inner child, the call, and the batch's
   table built from the inner child's batches; then the batch is
   probed as now.

The table of a batch reuses its memory: its records and index are
cleared, not freed, so a batch does not pay for a chunk of 64 kB and
an index again; values copied from the inner side are freed with each
batch. The batch's table never spills; what passes `hash_mem` is
counted as the join's overrun.

What assumes one table for the whole join is switched off in the mode
for good, not once: compact batches, which keep offsets of records
across outer batches; the Bloom filter, whose decision every new table
makes again and whose hand-down would drop the next batch's rows; the
pruning of a partitioned outer side by the table's keys, whose plan
data the planner does not write; the shared table and the spill. A
rescan of the join forgets the batch's table whatever parameter
changed. The table's per-build state (rows, duplicates, the NULLs of
its columns) is reset with each batch; the counters add up.

### Which joins

INNER and LEFT joins whose inner side passes the heap scan's
eligibility, with an index of `innerrel->indexlist` that is a btree
(`amsearcharray`), not partial, not hypothetical, whose first column is
a column of the table (not an expression) and the inner side of one of
the join's equalities. The operator, turned to have the index column on
its left (`get_commutator`), must be in the index's operator family
(`op_in_opfamily`) with the equality strategy
(`get_op_opfamily_strategy`), as the core matches a clause to an index
column; its input collation must match the index's
(`IndexCollMatchesExprColl`), and the clause must pass the rule for
promoting a clause into an index scan
(`restriction_is_securely_promotable`). The key may be of any type the
join takes. A key the join hashes from its value has one type on both
sides and the type's own equality, so text, numeric or char(n) keys are
of one type, and the join's residual equality decides their pairs as
now; an outer side of another type is possible only for the word
equalities of integers, dates and timestamps, and the family must hold
both comparisons a btree array key of that type needs (the index's type
against the outer's, and the outer's against itself). A nondeterministic
collation gets no lookup: its statistics do not count the values it
takes as equal. That equality is the lookup; the join's other equalities
and clauses stay its keys and residual clauses, and LEFT's filters above
it stay apart.

The planner offers the mode only when a batch's table fits a quarter
of `hash_mem` by the statistics of the inner key: 64 keys times the
rows of its most frequent value (the first of its most common values,
or one over the number of distinct values when it has none), times the
bytes of a record and of the values it copies, with the first chunk
of the table's records, its index for those rows and the rounding of
its chunks. A column without statistics gets no lookup.

### The cost

The cost counts what a lookup reads and what it keeps apart:

- the outer batches with a key, from the shape of the outer side: a
  scan's batches and the share its clauses keep (sparse batches count
  as batches), a join's rounds, or, where nothing better is known, a
  batch a row;
- per batch, a fixed price for the array, the call, the index's rescan
  and preprocessing, and the table;
- the descents: one for each distinct key of a batch, fewer where the
  keys of a batch lie close in the index, which the correlation of the
  outer key column says for a scan in the table's order; keys repeated
  across batches are looked up again each time;
- the inner rows fetched (the lookup equality's rows), the rows kept
  after the inner side's clauses, and the pairs after the join's;
- the start: the first outer batch, its lookup and its table before the
  first row.

None of the hash join's prices for compact batches or for pruning
apply. The prices are calibrated by a benchmark against the core's
nested loop, as `tess_join_cost_unit` was against the core's hash join.
The join's path keeps no order.

### Under a Gather

The outer side's partial path, and in each participant a lookup scan
of its own, not parallel-aware, with no shared table; the join keeps
its shared counters.

### EXPLAIN

On the join, `Inner Lookups: per outer batch`; with `ANALYZE`,
`Lookups` (the array scans), `Lookup Keys` (the keys given, duplicates
included, NULLs not), `Index Searches` (the descents the btree made)
and `Overrun`: for each participant the most any batch's table took
past the `hash_mem` in force when it was built. In a parallel plan each
is the sum of the participants', as the join's other lines are sums:
the join counts the descents, since the lookup scan, not parallel-aware,
shares no counters. With `VERBOSE`, `Builds`, which counts the tables
of batches with keys. On the lookup scan, `Lookup Index`
(`explain_get_index_name`) and `Lookup Key`.

## Risks / Trade-offs

- A key with more rows than its statistics say makes a batch's table
  larger than `hash_mem`; it is built anyway, counted, and may take
  memory the planner did not expect: the admission is an estimate,
  not a bound.
- The keys of a batch spread over the index: a descent a key, as in
  the core, and the batch's fixed price on top; the cost takes it.
- An index out of the table's order: each row a page, as in the core.
- A join of few outer batches: the fixed price of a lookup against a
  few row lookups; the cost takes it.

## Measurement

A benchmark family of index nested loops, calibrated and then measured
base against change: a selective outer side against a large table by its
primary key, one row a key and four; a skewed key; outer keys repeated
across batches, where the core may take Memoize; outer keys in index
order and shuffled; dense and sparse outer batches; a LEFT join with
misses; a join of a few outer rows; text and date keys. TPC-H Q3, Q5,
Q7, Q8 and Q21, base and change twice in turn, serial and with two
workers, Q1 and Q6 as controls; a profile of Q7 with the mode.
