# partition-pruning Specification

## Purpose
Skipping the partitions of a table that a query cannot need while the
query runs. TessAppend, which reads the children of an `Append` in
batches, prunes them by the query's parameters as the core's `Append`
does; a hash join prunes the partitions of its outer side by the keys
of the table it built, which the core does not do. The requirements go
from TessAppend's own pruning, when it starts, while it runs, in which
relations and in a parallel plan, to the join's: when it prunes, by
which keys, over rescans and in parallel, what EXPLAIN shows, how the
planner expects it, and a pruning join under a gather. Pruning by
constants while planning is the core's alone.
[design.md](design.md) explains the whole and the reasons.

## Requirements

### Requirement: Pruning when a node starts
TessAppend, the node that reads the children of an `Append` in batches,
SHALL prune its children that are partitions as the core's `Append`
does when execution starts: by the query's parameters, such as a
generic plan's `$1`, and by stable functions, such as `now()`, wherever
`enable_partition_pruning` is on and the relation's clauses on its
partition key allow it. A partition pruned then is not started. EXPLAIN,
with or without ANALYZE, SHALL show how many were pruned as
`Subplans Removed`: in the text format when the count is above 0, in
the other formats always, as for the core's `Append`. A node left with
no partition SHALL return no rows. `EXPLAIN (GENERIC_PLAN)` prunes
none.

#### Scenario: A generic plan's parameter
- **WHEN** a prepared query over four partitions, one of them
  partitioned again, runs a generic plan whose `$1` leaves two, one and
  none of them
- **THEN** EXPLAIN shows `Subplans Removed` 2, 3 and 4, the results are
  the core's, and the last returns no rows
- **Verified by:**
  `test/sql/union.sql::At the start, by a generic plan's parameter`

#### Scenario: A stable function, and a plan without the values
- **WHEN** a clause compares the key with a stable function's value, and
  `EXPLAIN (GENERIC_PLAN)` shows a query with `$1`
- **THEN** the first prunes as the parameter did, and the second prunes
  nothing
- **Verified by:**
  `test/sql/union.sql::At the start, by a stable function's value.`;
  `test/sql/union.sql::Planned without the values, nothing is pruned.`

#### Scenario: Every format, and pruning off
- **WHEN** a query that prunes nothing is shown in JSON, and a query
  that would prune runs with `enable_partition_pruning` off
- **THEN** the JSON has `"Subplans Removed": 0`, and the second reads
  every partition and shows no such line
- **Verified by:**
  `test/sql/union.sql::In a format other than text the line is shown when`;
  `test/sql/union.sql::With enable_partition_pruning off, nothing is pruned.`

### Requirement: Pruning while a node runs
TessAppend SHALL prune its partitions again at its first read, by the
parameters of execution: an initplan's value, or the parameter of a
correlated subquery. After a rescan that changed those parameters it
SHALL prune anew; a rescan that changed only other parameters SHALL
keep the partitions it found. A partition pruned while running is not
read, and EXPLAIN ANALYZE shows it `never executed`.

#### Scenario: An initplan's value
- **WHEN** a clause compares the key with an initplan's value
- **THEN** the partitions the value excludes are `never executed`, and
  the results are the core's
- **Verified by:**
  `test/sql/union.sql::At the first read, by an InitPlan's value.`

#### Scenario: A correlated subquery's parameter
- **WHEN** a correlated subquery compares the key with its parameter
  for five values, and another one compares the key with an initplan's
  value and another column with its parameter
- **THEN** in the first each partition is read only for the values that
  can reach it, and in the second the partitions the initplan's value
  excluded stay unread in every loop
- **Verified by:**
  `test/sql/union.sql::Anew for every value of a correlated subquery's`;
  `test/sql/union.sql::A rescan that changes another parameter keeps the`

### Requirement: The relations a node prunes
TessAppend SHALL prune the partitions of a partitioned table at every
level of its partitioning, and the partitions of each partitioned table
of a `UNION ALL`, each by its own partition key. A child that is not a
partition, such as a plain table in a `UNION ALL`, SHALL never be
pruned. A node in a subquery or a sublink SHALL prune as one in the
main query.

#### Scenario: A UNION ALL
- **WHEN** a `UNION ALL` of two partitioned tables, and one of a
  partitioned table and a plain table, are pruned by an initplan
- **THEN** both tables of the first prune by their keys, the plain table
  of the second is read whole, and the results are the core's
- **Verified by:**
  `test/sql/union.sql::UNION ALL of two partitioned tables, two hierarchies`;
  `test/sql/union.sql::A plain table beside the partitions is no partition`

#### Scenario: A sublink and a CTE
- **WHEN** a sublink's query prunes by a stable function, and a
  materialized CTE that groups its rows by an initplan
- **THEN** both prune as the main query does, and the results are the
  core's
- **Verified by:**
  `test/sql/union.sql::In a subquery of its own, whose range table the`

### Requirement: Pruning in a parallel plan
In a parallel plan every participant of a TessAppend SHALL read only
the partitions left for the current parameters, and no participant
SHALL read a partition pruned. After a rescan of the parallel plan the
partitions SHALL be pruned anew. A plan left with no partition SHALL
return no rows.

#### Scenario: Shared out among the participants
- **WHEN** a parallel plan prunes by a stable function, by an
  initplan, by a generic plan's `$1`, and by an initplan with the
  leader not taking part
- **THEN** the partitions pruned are `never executed`, and the results
  are the core's
- **Verified by:**
  `test/sql/union.sql::Pruned while executing: at the start by a stable`;
  `test/sql/union.sql::By a generic plan's parameter, the serial plan's rows`;
  `test/sql/union.sql::-- Without the leader.`

#### Scenario: A rescan, and nothing left
- **WHEN** a parallel plan is rescanned under a join, and a parallel
  plan prunes every partition
- **THEN** each scan reads only the partitions left, and the second
  returns no rows
- **Verified by:**
  `test/sql/union.sql::Pruned by an InitPlan: the leader finds the valid`;
  `test/sql/union.sql::initplan that leaves no partition, no rows.`

### Requirement: When a hash join prunes its outer side
A hash join of Tessera SHALL prune the partitions of its outer side by
the keys of its table when all of the following hold, and SHALL not
otherwise:

- `enable_partition_pruning` is on;
- the outer side is a TessAppend over the partitions of one partitioned
  table;
- the join is INNER, SEMI or RIGHT, which return no outer row without a
  pair; LEFT, ANTI and FULL joins never prune;
- a key of the join has one of the types int2, int4, int8, date, bool,
  timestamp and timestamptz, and its outer side is a column that is the
  first column of the partition key of the table, or of one of its
  partitioned partitions, compared by an operator of that key's family;
  a hash partitioning's key has that column alone.

When several keys can prune, the first one in the join's order SHALL.

#### Scenario: Joins that keep an outer row without a pair
- **WHEN** SEMI, RIGHT, LEFT and ANTI joins have keys that reach one
  partition
- **THEN** the SEMI and the RIGHT join prune the others, the LEFT and
  the ANTI join read them all, and the results are the core's
- **Verified by:**
  `test/sql/join.sql::SEMI and RIGHT keep no outer row without a pair and prune`

#### Scenario: The key that prunes
- **WHEN** a join has two keys, the partition key second; a join's key
  is the second column of a partition key; and a join prunes with
  `enable_partition_pruning` off
- **THEN** the first prunes by its second key, and the others prune
  nothing
- **Verified by:**
  `test/sql/join.sql::The key that prunes: the first that can, here the second`

### Requirement: The keys a hash join prunes by
A hash join that prunes SHALL, once its table is built and before it
reads any outer row, keep only the outer partitions that can hold a key
of its inner rows; a row with a NULL key holds none. While at most 1024
inner rows have a key, it SHALL keep the partitions of each of those
keys. Past that, it SHALL keep the partitions that can hold a value
between the lowest and the highest key, where the partitioning orders
the key; a hash partitioning then keeps every partition. A table without
a key SHALL keep no partition. A partition kept SHALL be read only
where TessAppend's own pruning keeps it too. The results SHALL be the
core's.

#### Scenario: A list of keys and their range
- **WHEN** inner sides of two keys, of 500 keys and of 2000 keys, of
  int4 and of int8, of 1024 and of 1025 rows with a key, reach range,
  list, hash and two-level partitionings
- **THEN** the lists keep the partitions of their keys, each partition
  read once, the ranges the partitions between their ends, a hash
  partitioning past its list every partition, and the results are the
  core's
- **Verified by:**
  `test/sql/join.sql::By the range of 2000 keys; by an int8 key; by two keys`;
  `test/sql/join.sql::The list's bound: 1024 rows with a key of jpr_1 or of`

#### Scenario: No key
- **WHEN** an INNER and a RIGHT join have inner sides whose keys are all
  NULL
- **THEN** both read no partition, and the RIGHT join returns its inner
  rows, as the core does
- **Verified by:**
  `test/sql/join.sql::Keys that are all NULL pair with no`

#### Scenario: With the node's own pruning
- **WHEN** a join's keys reach two partitions and an initplan's value in
  a clause of the outer side leaves one of them
- **THEN** only that one is read, and the results are the core's
- **Verified by:**
  `test/sql/join.sql::With the node's own pruning: the keys reach jpr_1 and`

### Requirement: Pruning over rescans and in parallel joins
A hash join SHALL prune by the keys of each table it builds: a table
kept over a rescan keeps the partitions it left, and a table built anew
prunes by its new keys. A table shared by the participants of a parallel
join SHALL prune by the keys of every participant, and every participant
SHALL read the same partitions; so SHALL tables that each participant
builds for itself. A table that spills to disk SHALL prune before it
reads any outer row, in one process and when shared.

#### Scenario: A rescan
- **WHEN** a subquery's join is rescanned for each row of a query, once
  keeping its table and once building it anew with keys in other
  partitions
- **THEN** each loop reads only the partitions its table's keys reach,
  and the results are the core's
- **Verified by:**
  `test/sql/join.sql::A rescan: a table kept for the outer side's new parameter`

#### Scenario: A parallel join
- **WHEN** a parallel join prunes over a shared table, over a table of
  each participant, and with the leader not taking part
- **THEN** no participant reads a partition pruned, and the results are
  the core's
- **Verified by:**
  `test/sql/join.sql::Pruning by the join's keys in parallel: every`;
  `test/sql/join.sql::Without the leader only the workers prune`

#### Scenario: A table that spills
- **WHEN** a table spills to disk in one process and when shared
- **THEN** both prune as a table in memory does, and no participant
  reads a partition pruned
- **Verified by:**
  `test/sql/join.sql::A table that spills keeps its range.`;
  `test/sql/join.sql::A shared table that spills reads the outer side in the`

### Requirement: What EXPLAIN shows of a join's pruning
With ANALYZE, a TessAppend that a hash join prunes SHALL show
`Subplans Removed by Join`: how many of the partitions it started the
keys of the join's last table removed. In a parallel plan the count
SHALL be the same whichever participants pruned, the leader among them
or not. A partition removed shows `never executed`. Without ANALYZE the
line SHALL not be shown.

#### Scenario: One process
- **WHEN** a join's keys reach one of four partitions
- **THEN** EXPLAIN ANALYZE shows `Subplans Removed by Join: 3` and the
  three `never executed`, and EXPLAIN without ANALYZE of a pruning join
  shows no such line
- **Verified by:**
  `test/sql/join.sql::the table is built, before the outer side is read`;
  `test/sql/join.sql::The planner expects the pruning. A side larger`

#### Scenario: A parallel plan
- **WHEN** a parallel join over a shared table reaches one partition,
  with and without the leader
- **THEN** both show `Subplans Removed by Join: 3`
- **Verified by:**
  `test/sql/join.sql::Pruning by the join's keys in parallel: every`;
  `test/sql/join.sql::Without the leader only the workers prune`

### Requirement: The planner expects the pruning
The planner SHALL count the cost of reading a pruning join's outer side
as that of the partitions it expects to be left, and may then choose a
plan it would not choose for the whole side. It SHALL expect the
partitions that can hold an inner key within what it knows before
execution: the lowest and the highest value of the inner column's
statistics, and the inner relation's clauses that compare the column
with a constant, either way round, at every level of the partitioning.
A hash partitioning, an inner key that is not a column, and clauses of
the inner relation on other columns give no expectation: the whole side
is costed. An inner clause that sets the key equal to a constant needs
none: PostgreSQL carries it over to the outer key and prunes by it
while planning.

#### Scenario: Statistics and clauses
- **WHEN** an inner side larger than the outer one has keys in one
  partition by its statistics, by a clause either way round, and at the
  second level of a partitioning
- **THEN** the planner builds the inner side and probes it with the
  outer side, which prunes to that partition
- **Verified by:**
  `test/sql/join.sql::The planner expects the pruning. A side larger`

#### Scenario: No expectation
- **WHEN** the inner keys reach every partition, and a hash partitioning
  is joined with an inner side whose keys reach one partition
- **THEN** in both the outer side is costed whole, and the smaller side
  is built
- **Verified by:**
  `test/sql/join.sql::a hash partitioning expects none,`

### Requirement: A pruning join under a gather
In a parallel plan, a hash join that expects to prune its outer side
SHALL be offered an outer side whose partitions every participant
divides with the others. An outer side whose participants take whole
partitions SHALL be offered beside it only where at least as many
partitions are expected to be left as there are participants.

#### Scenario: One partition left, and every one
- **WHEN** a parallel join expects one of four partitions to be left,
  and another expects every one, a worker's start counted in the cost of
  a scan
- **THEN** the first divides the partition among the participants, the
  second gives whole partitions, and no participant reads a partition
  pruned
- **Verified by:**
  `test/sql/join.sql::partial scans' cost, the core's Append`
