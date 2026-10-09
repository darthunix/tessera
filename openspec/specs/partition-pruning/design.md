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
the key's hash. A partition can itself be a partitioned table, split by
the same key or by another one: the partitioning then has **levels**.

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
TessAppend must prune the same partitions the core's `Append` prunes,
or Tessera would make such a query slower than the core. Take four
partitions of 1000 keys each and `WHERE k > $1` with `$1` = 2500: the
core's `Append` reads the two partitions above 2500, and a TessAppend
that did not prune would read all four.

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

**Range table numbers.** A pruning description names the relations it
prunes by their numbers in the query's **range table**, the list of the
relations the query reads. When the node is in a subquery, PostgreSQL
appends the subquery's range table to the statement's at the end of
planning, and every number of the subquery grows by the same offset.
PostgreSQL shifts the numbers it knows of, the node's relation among
them, but not those in the node's plan data. So when the node starts,
it shifts a copy of its description itself:

```
 the subquery's range table   1 union_part   2 union_part_1   3 ...
 the statement's              5 union_part   6 union_part_1   7 ...

 offset = the node's relation now (5) - in its description (1) = 4
```

If the shifted numbers do not name the node's relation, the plan is
broken, and the node raises an internal error.

**Borrowing the executor's list.** PostgreSQL makes the pruning states
of a statement once, before any node starts, from the statement's list
of descriptions (`ExecDoInitialPruning`), and hands each node its own
by its place in that list (`ExecInitPartitionExecPruning`). TessAppend's
description is not in the list: PostgreSQL puts only those of `Append`
and `MergeAppend` there. So the node lends the executor a list of its
own for the two calls:

```
 executor's lists         at the node's start
 the statement's ──save──► the node's description alone
 descriptions                │ ExecDoInitialPruning: initial pruning
                             │ ExecInitPartitionExecPruning: its state
                ◄──restore───┘ the children left
```

The lists are put back even when a call fails. The node starts only
the children left, numbered anew from 0, as the core's `Append` does;
when none is left, it returns no rows.

EXPLAIN shows the children the initial pruning removed as
`Subplans Removed`, as for the core's `Append`: in the text format
when there are any, and in the other formats always, so that a tool
reading JSON finds the same key on both nodes.

**Refused:** a hook in the core's `set_plan_references` to register the
node's description would need a change of the core. A pruning of the
node's own would have to follow every rule of the core's, and would
drift from it.

### Pruning while running

Some values are known only while the query runs: an initplan's result,
or a correlated subquery's parameter, which changes with every row of
the outer query. When the node's description has steps over such
values, the node prunes at its first read, before it reads any child,
and remembers the children left (`ExecFindMatchingSubPlans`). When the
node is rescanned:

- if a value its steps read has changed, it forgets the children and
  prunes again at its next read;
- if only other values have changed, it keeps them, as the core's
  `Append` does.

In `SELECT g, (SELECT count(*) FROM t WHERE k > g * 1000) FROM
generate_series(0, 4) AS g` the subquery runs five times, and each run
reads only the partitions above its own `g * 1000`.

### In a parallel plan

In a parallel plan several processes, the **participants**, read the
node's children at once. They share the children out through a small
area of shared memory: a flag for each child that needs no more
readers, under a lock. Pruning sets these flags too:

```
 the node's shared memory               the participants
 ┌─────────┬─────────┬─────────┬─────────┐
 │ child 1 │ child 2 │ child 3 │ child 4 │  1. each one, at its first
 │ done    │         │ done    │         │     choice, prunes under the
 └─────────┴─────────┴─────────┴─────────┘     lock and marks the pruned
   pruned              pruned                  children done
                                            2. every participant takes
                                               only children not done:
                                               2 and 4
```

When the parallel plan is rescanned, its shared memory is reset and
every flag is cleared. The leader then forgets the children it found,
so that the first choice prunes again for the new values. The core's
`Append` needs no such step: it leaves the pruning to the workers,
which are new in every scan.

## The hash join: pruning by its keys

### When a join prunes

The planner decides when it builds the join's plan, after the plans of
its children: only then does it know that the outer child is
TessAppend. The conditions are in [When a hash join prunes its outer
side](spec.md#requirement-when-a-hash-join-prunes-its-outer-side). Two
of them need a reason:

- **The join type.** An INNER, SEMI or RIGHT join returns no outer row
  without a pair, so an outer partition without a pair can be skipped.
  A LEFT, ANTI or FULL join returns such rows, and must read them.
- **The key.** The join hands TessAppend the values of its keys, and
  the core's pruning compares those values with the partition bounds.
  So the key must be a value the join keeps whole: an integer, a date,
  a boolean or a timestamp, which fit a word of 8 bytes. A key the join
  keeps only as a hash, such as text, has no value to compare. The key
  must also be the first column of a partition key, at the top level or
  a lower one: the core's pruning matches a clause to the partition key
  column by column from the first, and a clause on a later column alone
  prunes nothing.

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

**Refused:** changing the core's pruning so that it reads a column of
the inner side directly. That needs a change of the core, and Tessera
leaves the core as it is.

### The keys of a build

While the join builds its table, it notes the key of each inner row.
It finds no partitions then: it only keeps the keys, and the partitions
are found once the table is built (below). It keeps them in two forms:

- a **list** of the keys, while at most 1024 inner rows have a key;
- the **lowest and the highest** key, always; past 1024 rows only these
  two remain.

A NULL key pairs with nothing, and is not noted.

```
 the inner rows' keys   5   15005   NULL   5
 noted                  list: 5, 15005, 5   lowest 5   highest 15005
```

In a parallel join over a shared table each participant builds a part
of the table, and sees only some of the keys. Each one notes its own,
then adds them to one list in the join's shared memory, under a lock,
before the barrier that ends the build. Past the barrier every
participant reads the whole list:

```
 participant A notes 5, 15005  ─┐ added under the lock,
 participant B notes 7         ─┤ before the build's barrier
 participant C notes 20003     ─┘
                                 ▼
 the shared list: 5, 15005, 7, 20003 ──► past the barrier every
                                         participant prunes by it
```

A participant that joins after the build reads the list too. Where
each participant builds a whole table of its own, each one sees every
key, and the lists are the same.

### Pruning before the first outer row

Once the table is built, and before the join reads an outer row, it
hands its noted keys to TessAppend, which turns them into the
partitions to read with the core's pruning steps:

```
 the noted keys        TessAppend runs the steps of `key = $p`
 list: 5, 15005  ──►   $p = 5      → partition 1
                       $p = 15005  → partition 2
                       read: partitions 1 and 2; not 3, not the default
```

- With a list, it sets `$p` to each distinct key in turn, runs the
  steps, and adds up the partitions each key leaves. It stops early
  once every partition is needed.
- Past the list, it sets `$lo` and `$hi` to the lowest and the highest
  key and runs the steps of the range once. A hash partitioning has no
  range: then every partition is read.
- With no key at all, no partition is read.

The node then reads only the partitions that both this pruning and its
own left. A table kept over a rescan keeps its partitions; a table
built anew hands its new keys before its first outer row.

A shared table that spills to disk reads its outer side while the
build goes on, to send each outer row to its partition on disk. It
hands its keys down right after the build's barrier, before that read
begins.

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
relation with them, level by level. A hash partitioning has no order, so
no range of the inner keys prunes it, and the planner expects nothing of
it. An inner clause that sets the key equal to a constant needs no
expectation: the core carries it over to the outer key itself, and
prunes by it while planning. An inner key that is an expression, not a
column, gives no expectation, nor do clauses on other columns of the
inner relation, such as a calendar's `year = 2024`: nothing ties them to
the key.

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
- **Keys of other types.** A key that the join keeps only by its hash,
  such as text, does not prune: the join keeps no value to compare
  with the bounds. Other systems keep the values of any ordered type.
  Trino collects a build side's distinct values while they are few,
  and their lowest and highest past a limit, strings among them;
  DuckDB passes a list of the values for a small build side and their
  lowest and highest otherwise; Spark hands the build side's keys to
  the scan as a list. Tessera could do the same for text: copy the
  values for the list, and compare them by the column's collation for
  the ends. That costs a copy and a comparison through the type's
  function an inner row.
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
  partitioned and a plain table; a list partitioning with a default
  partition and a hash one; a sublink; parallel plans, by every
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
  a second level, none for a hash partitioning; the divided `Append`
  under a gather.
