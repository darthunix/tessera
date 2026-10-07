# key-filter: how it is built

A hash join reads its outer side, the probe side, and looks each row up
in a table of its inner side. When most outer rows find no pair, the
join builds a Bloom filter of its table's keys and checks the rows
against it before it looks them up (capability
[hash-table](../hash-table/design.md)). By then the node below the join
has already done its work on those rows, and some of that work is
costly: a TessFilter runs some of its clauses one row at a time, through
PostgreSQL's executor. A key filter lets the join hand its filter down,
so that the child drops the rows that cannot find a pair before its
costly clauses run.

This page explains the idea and how it is built. The exact rules, and
what EXPLAIN shows, are in the capability's spec,
[spec.md](spec.md), next to this page.

## Background

A **batch** is up to 64 rows of a node, kept by column, with a **mask**
of the rows still taken: a bit a row. A node removes a row from a batch
by clearing its bit.

A **TessFilter** applies a relation's conditions to the batches of the
scan below it. A condition the kernels can compute runs on a whole
batch at once, a **batch clause**. Any other runs row by row: PostgreSQL
evaluates it on each row, a **row-wise clause**, which costs far more a
row. The planner keeps the clauses in their order, so a cheap guard
stays in front of the clause it protects.

A **Bloom filter** is a small array of bits made from the keys of a
table. A key that is not in the table usually fails the check; a key
that is in it always passes. The join hashes a row's keys and checks
the hash; a row that fails has no pair.

## The problem

1. A row-wise clause runs through PostgreSQL's expression machinery for
   each row, and its work on a row without a pair is wasted. A check of
   the filter is a read of one word. How does the work move below the
   join, to the node that runs the clauses?
2. The join and the node below are separate nodes, and the node below
   may come from another extension. How does a parent tell a child about
   its filter, who owns the memory, and what if the child cannot use it?
3. Which rows may a child drop without changing the join's result?
4. Once rows are dropped below, how does EXPLAIN say where they went?

## Goals and what they cost

- **Drop the rows early** (question 1). The child checks the filter
  before its first row-wise clause. The price is a hash of the keys in
  the child for every row it checks, and the join hashes the rows that
  pass once more to probe its table.
- **An offer the child may refuse** (question 2). A node kind may offer
  a callback that takes a filter; a parent offers its filter through the
  runtime, and keeps checking rows itself when the child refuses. A
  parent owns the filter's words until it takes them back; the child
  copies everything else it keeps.
- **Only where the result does not notice** (question 3). A join hands
  its filter down only when a row without a pair leaves its output.
- **One name, one meaning** (question 4). A line of EXPLAIN counts the
  same rows on the join and on the child, and the rows dropped for
  another reason have a line of their own.

## The whole in one picture

```
 TessHashJoin, INNER          its table and its Bloom filter
   │ the probe side                 │
   │                                │ the filter's words, the join's
   ▼                                ▼ until it takes them back
 TessFilter on the probe side ◄─────┘
   each batch:  batch clauses  →  key filter  →  row-wise clauses
                                  │
                                  ├─ a NULL key: removed
                                  └─ a hash the filter rejects: removed
   ▼
 TessHeapScan on the probe side
```

Over a partitioned outer side, TessAppend forwards the filter to the
node of each partition, in that partition's own columns:

```
 TessHashJoin
   ▼
 TessAppend ── the filter, columns mapped ──► TessFilter on partition 1
                                          └─► TessFilter on partition 2
```

## The call

A node kind of the node contract (`tessera/node.h`) may offer
`set_key_filter`. A parent calls `tess_input_set_key_filter` on its
input, and the runtime finds the child's kind and calls the callback,
or answers that the child does not take filters. The callback answers
whether the child took the filter; a parent takes it back by handing
`NULL`. The rules, and a drawing of `TessKeyFilter`, are in
[A key filter and its call](spec.md#requirement-a-key-filter-and-its-call).

The struct of the filter and its arrays of columns and kinds are valid
only during the call, and a child copies what it keeps of them. The
parents build them where they please: the join keeps the struct on its
stack, and TessAppend maps the columns of each partition into one array
of its stack, used again for the next. Only the words of the filter
must outlive the call, and they are the parent's: in its memory context,
or in the query's shared memory when the table is shared.

A child that refuses keeps what it had before the call: a filter it
held stays as it was.

## Who hands a filter down

A hash join decides on its filter after its first 4096 probe rows, or
at once when `tessera.join_bloom_ratio` is 1, and offers it to its
outer child right after it builds it, when three things hold:

- **The join drops a row without a pair.** INNER and SEMI joins do, and
  so does a RIGHT join, which keeps the rows of its inner side and drops
  outer rows without a pair. ANTI and LEFT joins return such a row,
  FULL too, so they keep their filter and check rows themselves.
- **The table does not spill.** A table that spills holds only part of
  its rows in memory, and its filter knows only those.
- **Every key is a word.** A key of another type is hashed from its
  value (capability `hash-table`), and the child holds the value, not
  that hash.

Once a child took the filter, the join checks no more rows: they come
already filtered. The join takes the filter back before the table it
was built from goes: before a new table for a parameter, before a
shared table is freed, and before a participant leaves a shared table,
whose words the last participant to leave frees. A rescan that keeps
the table keeps the filter below.

## Who takes a filter

- **TessFilter** takes a filter when it has a row-wise clause: without
  one, a check in the child saves nothing over the join's own. The keys
  must be columns of its child, since a column the node computes exists
  only after its clauses ran, and of the two kinds the table keeps as
  words.
- **TessAppend** forwards the filter to the node of each partition, with
  the keys mapped to the partition's own columns, and takes it only when
  every partition does: the join checks no more rows once a node below
  took the filter, so a partition that refused would pass rows nobody
  checks. When one refuses, the partitions that took it give it back.

The rules are in [Who takes a
filter](spec.md#requirement-who-takes-a-filter).

## What the child removes

TessFilter runs its clauses in the planner's order. It checks the filter
once a batch, right before its first row-wise clause, after the batch
clauses before it. It hashes the keys of the rows still taken as the
join hashes them, and removes two kinds of row:

- a row with a NULL key, which equals no key, so it has no pair;
- a row whose hash the filter rejects.

A removed row reaches none of the later clauses, so an error such a
clause would raise on it is not raised. PostgreSQL does not fix the
order in which it evaluates conditions either.

A shared filter is built by one participant while the others probe
without it. The child of each participant checks the filter only once
it has seen it ready; until then every row passes, and the join's probe
finds the pairs.

## What EXPLAIN shows

A row that cannot find a pair may be removed in three places: by the
join's own check of its filter, by the child's check, or by the child
for a NULL key. The join shows the first as
`Rows Removed by Bloom Filter`, with `Bloom Filter Below` when a child
took its filter. The child shows the second by the same name, and the
third as `Rows Removed by NULL Key`. The join cannot count the rows with
a NULL key as removed: a LEFT or ANTI join returns them. The lines are
in [The rows a filter removes, as EXPLAIN shows
them](spec.md#requirement-the-rows-a-filter-removes-as-explain-shows-them).

The join's lines are totals over its loops, as its other counters are;
TessFilter's are per loop, as the core shows the rows its own
conditions removed. Every row a TessFilter removes also counts in the
core's `Rows Removed by Filter` of that node.

## What we decided not to do

- **One line for every row the key check removes.** TessFilter counted
  the rows with a NULL key as removed by the Bloom filter, so the same
  line counted different rows on the join and below it.
- **The struct valid until taken back.** Each parent would keep a copy
  in its state for as long as the filter is below; the one child copies
  what it needs instead.
- **A filter for ANTI, LEFT and FULL joins.** They return the outer
  rows without a pair, so a child may not drop them.

## Files

- `include/tessera/node.h`: the key filter and the callback of a kind
- `include/tessera/runtime_input.h`, `runtime/input.c`: the call
  through the runtime
- `runtime/qual.c`: the place of the check among a TessFilter's clauses
- `nodes/filter.c`: TessFilter takes a filter, applies it, and counts
  its rows
- `nodes/append.c`: TessAppend forwards a filter
- `nodes/hashjoin.c`: the join hands its filter down and takes it back
- `nodes/hashjoin_shared.c`: a participant takes it back before it leaves
- `nodes/hashjoin_begin.c`: the join's lines of EXPLAIN

## Tests

- `test/sql/join.sql`: joins of every type over a TessFilter, alone,
  shared and over TessAppend, with their plans, their rows against the
  core's and the lines of EXPLAIN.
- `test/sql/unary.sql` with `test/tessera_unary_test.c`: the call
  through the runtime, a kind that refuses, and a filter too small.
