# partition-pruning: how it is built

This page explains how Tessera skips the partitions of a table that a
query cannot need while the query runs. The rules are in
[spec.md](spec.md); this page says why they are what they are and how
the code keeps them.

## Background: partitions and pruning

A **partitioned table** is a table split into **partitions**, each a
table of its own, by the value of its **partition key**. A range
partitioning gives each partition a range of the key, a list
partitioning a list of values, and a hash partitioning a remainder of
the key's hash. A partition may be partitioned again, by the same key
or another: the partitioning then has **levels**.

To read a partitioned table, PostgreSQL plans an `Append`: one child
for each partition, read one after another. **Pruning** leaves out the
children whose partitions cannot hold a row the query needs. The core
prunes at three moments:

- **While planning**, by constants: `WHERE k > 2500` drops the
  partitions below 2500 from the plan. Tessera never sees them.
- **When execution starts** (initial pruning), by the values the query
  has from the start: a generic plan's parameter `$1`, or a stable
  function such as `now()`. A child pruned then is not even started.
- **While running** (pruning by the parameters of execution), by values
  known only later: an initplan's result, or a correlated subquery's
  parameter, which changes with each row of the outer query. A child
  pruned then is not read, and is pruned anew when the values change.

For the last two the planner writes a **pruning description**: the
clauses on the key, turned into steps over the partition bounds. At
execution the core's **pruning state** runs the steps with the current
values and answers which children are left. Only the core's `Append`
and `MergeAppend` do this. EXPLAIN shows the children removed when
execution started as `Subplans Removed`, and those not read while
running as `never executed`.

## The problem

Tessera replaces the core's `Append` under a node that reads batches
with **TessAppend**, which hands its children's batches up as they are.
If TessAppend did not prune, a query with `k > $1` would read every
partition under Tessera and only some of them under the core.

A hash join could skip more. It first builds a **table** of its inner
side, then reads its outer side and looks each row up. Once the table
is built, the join knows every key an outer row can pair with. When the
outer side is a partitioned table, a partition that can hold none of
those keys has no row that pairs, and an inner join need not read it.
A star schema is the common case: a fact table partitioned by date,
joined with a few rows of a calendar. The core does not prune by a
hash join's keys at all.

## Goals and what they cost

- **TessAppend prunes as the core's `Append`.** It uses the core's own
  pruning, so the two always agree. The price is that the node calls
  the core in a way the core made for its own nodes: it lends the
  core's executor a list that holds only its own description (below).
- **A join prunes without changing the core.** The planner writes the
  core's ordinary descriptions over parameters that stand for the
  inner keys, and the join sets them after its build. The price is
  small and fixed: a comparison and a store for each inner row, and
  one run of the pruning steps for each distinct key of a short list.
- **The planner counts on the pruning.** A plan whose outer side will
  read one partition of four should look as cheap as it is. The price
  is an estimate, which can be wrong: it knows the inner keys only by
  their statistics and by constant clauses.
- **Every participant of a parallel plan prunes the same.** The price
  is a short list of keys in shared memory, filled under a lock.

## The whole in one picture

```
 planning                              execution
 ────────                              ─────────
 TessHashJoin                          1. TessAppend starts: initial
   plan data: key = $p,                   pruning by $1 and now()
              key >= $lo AND key <= $hi   leaves the children to start
   │                                   2. the join builds its table and
   │ outer                                notes the inner keys
   ▼                                   3. the join hands the keys down:
 TessAppend                               each key sets $p and prunes,
   plan data: its own description         or $lo, $hi prune as a range
   ├── partition 1                     4. at its first read TessAppend
   ├── partition 2                        prunes by the parameters of
   ├── partition 3                        execution, and reads the
   └── partition 4                        children both prunings left
```

The node's own pruning and the join's pruning are independent: each
gives a set of children, and the node reads those in both.

## TessAppend: pruning by the query's parameters

### The description

The planner makes TessAppend's description as it makes an `Append`'s,
with the core's `make_partition_pruneinfo`, from the relation's clauses
and the paths of its children. It does so for any partitioned table,
and for a `UNION ALL` of partitioned tables, whose children come from
several tables: each table is a hierarchy of its own and prunes by its
own key. A child that is not a partition, such as a plain table in a
`UNION ALL`, belongs to no hierarchy and is never pruned.

The core collects such descriptions in a list of the planner, and
`set_plan_references` carries only those of `Append` and `MergeAppend`
into the statement. TessAppend is a custom scan, so it takes its
description off that list and keeps it in its own plan data.

### Initial pruning

Range table numbers in a description are those of the query it was
planned in. When that query is a subquery, `set_plan_references`
shifts its range table into the statement's and adds the offset to the
node's relation numbers, but not to the plan data. So at its start the
node copies the description and adds the offset itself, the difference
between its relation's number now and in the description. The node
checks that the shifted numbers are its relation's, and fails with an
internal error otherwise.

The core makes the pruning states of the statement's descriptions only,
before any node starts (`ExecDoInitialPruning`), and gives a node its
own by number (`ExecInitPartitionExecPruning`). The node has no number
in that list. It replaces the executor's lists with lists that hold its
description alone, makes both calls, and puts the lists back, also on
an error. The first call runs the initial pruning; the second gives the
node its state and the children left. The node starts only those, and
numbers them anew from 0, as the core's `Append` does. If none is left,
the node returns no rows.

EXPLAIN shows the children the initial pruning removed as
`Subplans Removed`, as for the core's `Append`: in the text format
when there are any, and in the other formats always, so that a tool
reading JSON finds the same key on both nodes.

**Refused:** a hook in the core's `set_plan_references` to register the
node's description would need a change of the core. A pruning of the
node's own would have to follow every rule of the core's, and would
drift from it.

### Pruning while running

When the description has steps on parameters of execution, the node
finds the valid children at its first read
(`ExecFindMatchingSubPlans`) and reads only those. A rescan that
changed one of those parameters forgets them, and the next read finds
them anew; a rescan that changed only other parameters keeps them, as
the core's `Append` does.

### In a parallel plan

The participants share the children out through the node's chunk of
shared memory: a flag for each child that needs no more participants,
under a lock. The first participant to choose a child finds the valid
ones, under the lock, and sets the flag of every other child, so that
no participant reads a pruned one. When the parallel plan is rescanned,
its shared memory is reset, and the leader forgets the valid children
too, so that it finds them anew for the new values; the core's
`Append` leaves that to the workers, which are new in every scan.

## The hash join: pruning by its keys

### When a join prunes

The planner decides when it builds the join's plan, after the plans of
its children, as Greengage decides for its own partition selector. The
conditions are in [When a hash join prunes its outer
side](spec.md#requirement-when-a-hash-join-prunes-its-outer-side). Two
of them need a reason:

- **The join type.** An INNER, SEMI or RIGHT join returns no outer row
  without a pair, so an outer partition without a pair can be skipped.
  A LEFT, ANTI or FULL join returns such rows, and must read them.
- **The key.** The key must be a word: one of the types whose values
  the join keeps whole in a word of 8 bytes. The join hands down keys,
  not hashes, and the core's pruning compares them with the partition
  bounds. The outer side of the key must be the first column of the
  partition key, at the top or at a lower level, since the core's
  pruning matches a clause on a key column by column from the first.

### Descriptions over parameters

The core's pruning takes clauses of the form `key op value`, where the
value may be a constant or a parameter, but never a column of another
relation. So the planner makes three parameters of execution of the
join's own and writes two descriptions over the outer relation:

- `key = $p`, run once for each inner key;
- `key >= $lo AND key <= $hi`, run once for the range of the keys,
  where the key's btree family has the two operators. A hash
  partitioning has no order, and gets no range.

The descriptions live in the join's plan data, since the join's plan is
made after TessAppend's. When the join starts, it hands them to
TessAppend, which makes their pruning states as it makes its own, over
every child it planned.

**Refused:** Greengage changed the core's pruning to take a column of
the inner side. Tessera leaves the core as it is.

### The keys of a build

As the join reads its inner side, it notes the key of each row; a NULL
key pairs with nothing and is skipped. It keeps the lowest and the
highest key, and the keys themselves while at most 1024 rows have a
key. Past that it keeps only the two ends.

```
 inner keys     5  15005  NULL  5       rows with a key: 3
 noted          list: 5, 15005, 5        lowest 5, highest 15005
```

In a parallel join over a shared table, each participant notes the rows
it builds and adds them to a list in the join's shared memory, under a
lock, before the barrier that ends the build. Past the barrier every
participant reads the same list, so every participant prunes the same
children. A participant that comes after the build reads the list too.
A parallel join whose participants each build a whole table of their
own note the same keys.

### Pruning before the first outer row

Once its table is built, and before it reads an outer row, the join
hands its keys to TessAppend. A table shared by participants that
spills to disk reads its outer side in the phases of the build, to send
its rows to the partitions on disk; it hands the keys down past the
build's barrier, before that.

TessAppend then finds the children the keys can reach:

- with a list, it sets `$p` to each distinct key, runs the steps, and
  adds up the children each key leaves, stopping once every child is
  needed;
- past the list, it sets `$lo` and `$hi` to the ends and runs the
  range's steps; without a range, every child is kept;
- with no key at all, no child is kept.

For the example above, over a range partitioning of four partitions of
10 000 keys each, key 5 leaves the first partition and key 15005 the
second: two children are read and two are not.

The node reads only the children that both its own pruning and the
join's left. A table kept over a rescan keeps the children it pruned; a
table built anew hands its new keys before its first outer row.

The work is fixed and small: a comparison and a store for each inner
row while building, and one run of the steps for each distinct key, at
most 1024, after it. DuckDB passes a short list of keys and their range
in the same way.

**Refused:** pruning as each inner row is inserted, for every row of
the build, costs as much as the insertion itself. A patch of the core
that did so was withdrawn for that reason.

### What EXPLAIN shows

With ANALYZE, TessAppend shows `Subplans Removed by Join`: how many of
the children it started the join's keys removed. The count is the
node's state at the end of the query, and in a parallel plan the
leader's node may not have pruned at all: without the leader, only the
workers do. So every participant that prunes adds the count and a 1 to
two counters of the node, which are summed over the participants as
its batches are, and EXPLAIN shows the first divided by the second.
Every participant prunes the same children, so the quotient is the
count. Over rescans that build the table anew the line shows the count
of the last build.

## The planner's expectation

A join over a large outer side that prunes to one partition is cheaper
than the same join without pruning, and the planner should know it, or
it may build the large side and probe it with the small one. So the
planner **expects** the pruning: it guesses the partitions the inner
keys will reach before execution, from what it knows of them.

- **Statistics.** The lowest and the highest value of the inner
  column's statistics: the ends of its histogram and its most common
  values. They come from a sample, so a rare value outside them is
  missed.
- **Clauses.** The inner relation's clauses that compare the key
  column with a constant, either way round: `k > 20000` or
  `20000 < k`. The core carries an equality across a join to the other
  side, but no inequality, so the planner carries them itself.

It turns these into clauses on the outer key and runs the core's
planning-time pruning (`prune_append_rel_partitions`) over the outer
relation with them, level by level. A hash partitioning has no order:
the statistics say nothing of it, and only a clause that sets the key
equal to a constant gives it an expectation, the partition of that
value. An inner key that is an expression, not a column, gives no
expectation, nor do clauses on other columns of the inner relation,
such as a calendar's `year = 2024`: nothing ties them to the key.

For example, with four range partitions of `k` from 1 to 30 000 and a
default partition, and an inner side whose statistics say its keys run
from 20 001 to 30 000, the planner expects the third partition alone.

The planner then costs the core's `Append` over the partitions it
expects to be left, with the core's own costing, and scales the cost
of the join's outer child by the ratio of the two `Append`s. The
join's outer child itself keeps every partition, and prunes at
execution. The join's own terms, its probe and the rest, still count
the rows of every outer partition.

## Under a gather: dividing the partitions

In a parallel plan, the core's parallel `Append` gives a child whole to
one participant when that is cheaper than dividing it. Tessera's
partial scan counts a worker's start in its cost, which such an
`Append` adds up once for each partition, so it often prefers whole
partitions. When fewer partitions are expected to be left than there
are participants, the other participants then have nothing to read.

So a parallel join that expects to prune its outer side is offered an
outer side whose partitions every participant divides with the others:
an `Append` over the partial paths of the partitions. The core's
`Append` of whole partitions is offered beside it only where at least
as many partitions are expected to be left as there are participants,
and the cheaper one wins.

Measured over four partitions of 500 000 rows by ranges of the key,
joined with a dimension of 100 000 keys that reach the first one, in
medians: 16.1 ms without pruning, 5.3 ms with it, and 78.4 ms for the
core. With one partition of four left, a serial plan took 5.10 ms,
whole partitions under a gather 5.83 ms and divided ones 5.45 ms; with
five times the data 36.6, 34.4 and 23.6 ms.

## What we decided not to do

- **The join's own cost over the partitions left.** The probe, the
  batches, the Bloom filter and the spilling of a pruning join are
  still costed for every outer partition, so the join looks dearer than
  it is. Changing that changes which plans win, and needs to be
  measured first.
- **A list bounded by distinct keys.** The list of keys is bounded by
  the rows with a key, not by distinct keys: counting distinct keys
  would cost a set of them at the build.
- **Keys of other types.** A key that the join keeps by its hash, such
  as text, does not prune: the join would have to keep its values for
  the list, and their order for the range.
- **The core's fixing of plan references.** The steps in the node's
  plan data do not pass through `set_plan_references`, and the node's
  partitions do not join the statement's prunable relations. Both
  matter only for row marks and for statements that modify data, which
  the node never takes.
- **`MergeAppend`.** TessAppend stands only for `Append`, so an ordered
  read of partitions stays the core's, with the core's pruning.

## Files

- `nodes/append.c`: TessAppend's description, its pruning when it
  starts and while it runs, in a parallel plan, and by a join's keys,
  and the lines of EXPLAIN.
- `nodes/prune.h`: the keys a join hands down.
- `nodes/hashjoin.c`, `nodes/hashjoin_shared.c`: the keys of a build,
  shared by participants, and the moment they are handed down.
- `nodes/hashjoin_begin.c`: the descriptions read from the plan and
  handed to TessAppend at the start.
- `nodes/join_planner.c`: when a join prunes, its descriptions over
  parameters, the expectation, and the divided Append under a gather.

## Tests

- `test/sql/union.sql`: TessAppend's own pruning, compared with the
  core's results. When it starts, by a generic plan's parameter, by a
  stable function and in every format; while it runs, by an initplan
  and a correlated subquery's parameter, with a rescan that changes
  other parameters; a `UNION ALL` of partitioned tables and of a
  partitioned and a plain table; a sublink; parallel plans, by every
  kind of value, without the leader, rescanned, and with nothing left;
  `enable_partition_pruning` off.
- `test/sql/join.sql`: a join's pruning, compared with the core's
  results. Range, list, hash and two-level partitionings; keys of int4
  and int8; a list of keys at its bound and past it; join types that
  prune and do not; a join of two keys and a key on a second column;
  keys all NULL in an INNER and a RIGHT join; with the node's own
  pruning; rescans that keep and rebuild the table; a table that
  spills; a shared table, tables of each participant, and the leader
  not taking part; `Subplans Removed by Join` without the leader; the
  planner's expectation by statistics, by clauses either way round, at
  a second level, for a hash partitioning; the divided `Append` under a
  gather.
