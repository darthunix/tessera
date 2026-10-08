# partition-pruning: how it is built

## TessHashJoin: pruning the outer side's partitions

When the outer side is `TessAppend` over a partitioned table whose
partition key is the outer side of a key of words, and the join keeps no
outer row without a pair (inner, semi, and right joins, where the inner
side is kept), the table's keys tell which partitions can pair: the others
are not read. The core prunes at execution only by the parameters of an
`Append` (a nested loop's inner side, which a batch node leaves to the
core); a patch for hash joins (2023–2024) pruned for every row the build
inserted, which cost as much as the insertion, and was returned. The
planner (`write_join_prune` in `join_planner.c`, at the join's
`PlanCustomPath`, the children planned, as Greengage's
`joinpartprune.c` decides at `create_plan`) makes the core's pruning
descriptions for `key = $p` and, where the key's btree family compares
the two types, `key >= $lo AND key <= $hi`, over three parameters of
execution of its own (`assign_special_exec_param`): the core's pruning
takes no column of another relation, and the parameters stand for the
inner side's values, so the core is left as it is (Greengage changed
its pruning to take the inner side's columns). At the start the join
hands them to `TessAppend` (`tess_append_join_prune_begin`), which makes
their pruning states as its own over every planned child. The build
notes the key of each inner row, the NULL ones apart: the lowest and
the highest, and the keys themselves while the inner side has at most
1024 rows with one; a shared table's participants add theirs under a
lock before the build's barrier. Once built, before the outer side is
read (a shared table that spills reads it in the build's phases, to the
partitions on disk: past the barrier, there), the join hands the keys
down (`tess_append_join_prune`): each
distinct key of a list sets `$p` and prunes, the children added up,
stopping once every child is needed; past the list the range prunes,
where there is one; no key pairs with no partition. The work is a key
an inner row at the build and a pruning a distinct key after it, as
DuckDB passes a list of a few keys and their range. `TessAppend`
intersects the children it reads with those left and shows `Subplans
Removed by Join` with `ANALYZE`; the children kept are the same in every
participant. A table kept over a rescan keeps them; a table built anew
hands its own.

The planner expects the pruning (`expected_leaves`): by the same key
(`prune_key`, which the plan's pruning takes too), it bounds the inner
key's values by what it knows before execution, the lowest and the
highest of the column's statistics (the histogram's ends and the common
values) and its relation's clauses `key op constant`, carried over to
the outer key (the core carries no inequality across a join), and prunes
the outer relation by them with the core's pruning at planning
(`prune_append_rel_partitions`), level by level. The template of the
join's cost then reads the partitions left: a copy of the core's
`Append` path with their subpaths, costed by the core (`cost_append`);
the join's child keeps every partition and prunes at execution. A hash
partitioning expects nothing (the range prunes none of it), nor does a
dimension whose clauses are on other columns (a star's `year = 2024`):
the planner cannot tie them to the key's range, and the cost is the
whole side's, as before. Under a `Gather`, the core's parallel `Append`
may give a partition whole to one participant, since our partial scan's
cost carries a worker's start (`tessera.scan_parallel_setup_cost`),
which the `Append` adds up once a partition: with fewer partitions left
than participants, the others have nothing to read. A join expected to
prune its outer side takes an `Append` over the partitions' partial
paths instead, every partition divided among the participants
(`divided_append`); with at least as many left, the core's competes.
Measured (medians, ms): one partition of four left, serial 5.10, whole
partitions 5.83, divided 5.45; the same with five times the data 36.6,
34.4 and 23.6; every partition left 16.4, 10.8 and 10.4. Over `bench_part`, four
partitions of 500 000 rows by ranges of `k`, joined with a dimension of
100 000 keys that reach the first (bench/pg join, `part_prune`, 11 runs,
medians, the core's time after): 16.1 ms before, 5.3 after (78.4); with a
thousand keys 13.7 and 3.7 (65.4); with two workers 11.1 and 6.0 (39.8),
8.8 and 4.2 (27.9), where the planner, not counting the pruning, kept
the parallel plan the one partition left does not need; counting it,
the plan is serial there too, 5.4 and 4.0.

## TessAppend: pruning while executing

The core prunes partitions twice. While planning, by constants: the
partitions pruned get no path, so the node's children are those left.
While executing, only its `Append` and `MergeAppend` do, and so does the
node: at the start by the query's parameters and stable functions (a
generic plan's `$1`, `now()`), and at the first read by the parameters of
execution (an initplan's value, a correlated subquery's parameter),
again after a rescan that changed them. The planner makes the
description as `create_append_plan` does, `make_partition_pruneinfo`
over the relation's clauses and the children's paths (whose parents are
the partitions), for any base relation, a `UNION ALL` of partitioned
tables included (a hierarchy each); where it makes one, the node takes it
off the planner's list into its plan data, since `set_plan_references`
carries into the statement only those of `Append` and `MergeAppend`
(`register_partpruneinfo`). The description's range table numbers are
then the node's query's own: at the start the node shifts a copy by the
offset `set_plan_references` added to its `custom_relids`, which the core
fills with the relation's (a wrong shift is the core's error "trying to
open a pruned relation"). The core makes the pruning states of the
statement's descriptions only, before the nodes start
(`ExecDoInitialPruning`), and gives a node its own by number
(`ExecInitPartitionExecPruning`): the node calls both over the EState's
lists holding its description alone, and puts the lists back. The
children the initial pruning left are the only ones started, numbered
anew, the first partial of them found again, as `ExecInitAppend` does;
none may be left, and the node then gives nothing. With pruning by the
parameters of execution, the valid children (`ExecFindMatchingSubPlans`)
are found at the first read and again after a rescan whose changed
parameters are the pruning's, and the node goes over them alone. In a
parallel plan each participant finds them at its first choice, under the
lock, and finishes the others for all, as the core's
`mark_invalid_subplans_as_finished`; the leader finds them anew when the
shared memory is reset for a rescan, where the core's leaves that to the
workers, new in every scan. `EXPLAIN` shows the children the initial
pruning removed as `Subplans Removed`, as for the core's `Append`; the
children pruned while running are `never executed`. Before, such a
clause kept the core's `Append`, whose rows a pack made batches again,
and a `UNION ALL` of partitioned tables, whose parent has no partition
key, read every partition. Half of a partitioned table of 2 000 000
rows read through the node, serially: by a generic plan's parameter 11.6
ms before, 5.1 after, by an initplan 12.1 and 5.1, a `UNION ALL` of the
table with itself 14.6 and 5.1, the core's 29 (pg-setop-xT3wei,
pg-setop-LpWP7U); with two workers 7.4, 7.3 and 9.6 before, 4.8 to 4.9
after, the core's 13 (pg-setop-w2-xKCerq, pg-setop-w2-z1nyzp).

## TessAppend: a join's keys

A hash join above may prune the node's children by the keys of its table
(TessHashJoin, Pruning the outer side's partitions): it hands the node the
pruning descriptions it planned, whose states the node makes as its own,
and after its build the keys, by which the node finds the children left
before its first choice, their numbers the planned ones mapped to its
own; it intersects them with those of its own pruning, over a rescan
too, until the join's next build.

## Files

- `nodes/append.c`
- `nodes/hashjoin.c`
- `nodes/join_planner.c`

## Tests

Pruning the outer side's partitions: a range partitioning with a default
partition and a NULL key, by a list of keys and by the range of 2000,
by an int8 key, by two keys of two partitions, a list partitioning by
keys at its two ends, a hash one by listed keys and past them (none),
the second level of two; semi and right joins prune, left and anti ones
do not, keys all NULL prune every partition; a table kept for the outer
side's new parameter and one built anew for the inner side's, the
partitions no execution read counted; the planner's expectation, a side
larger than the partitioned one built over it when its keys reach one
partition by their statistics, by a clause on the inner key (either
way round), or at the second level, and not when its keys reach every
partition or the partitioning is by hash, and under the `Gather`, with
a worker's start in the scans' cost, the partitions divided among the
participants where one is left and whole where every one is; a spilling
table; under the
`Gather` a shared table, one that spills (the partitions no execution
read counted), and tables of every participant, the leader not taking
part. Mutations fail it: no pruning, every join type, the last
key's partitions alone, the range for a list, no shared keys, NULL keys
counted, no expectation, no clause carried over, no key at the second
level, the second level's clauses left out, the divided Append never,
the core's never beside it or always; the stop once every child
is needed leaves the results as they are.

`test/sql/union.sql`: partitions pruned while executing (the children
read shown by `EXPLAIN ANALYZE`): by a generic plan's parameter (two,
three and every child removed, none), with `EXPLAIN (GENERIC_PLAN)`
removing none, by a stable function, by an initplan, anew for each value
of a correlated subquery's parameter, a `UNION ALL` of two partitioned
tables, within a sublink and a materialized CTE (queries whose range
table the statement offsets). Mutations of the pruning fail it: no
initial pruning, no pruning while running, the valid children kept over
a rescan with new parameters, the invalid ones left unfinished in a
parallel plan or after its reset, the range table numbers unshifted.
