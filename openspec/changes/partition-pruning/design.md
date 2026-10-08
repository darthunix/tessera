## Context

TessAppend reads the children of an `Append` in batches
(`nodes/append.c`). It prunes the partitions among them as the core's
`Append` does: when it starts, by the query's parameters, and at its
first read, by the parameters of execution, through the core's pruning
state. A hash join (`nodes/hashjoin*.c`) may prune the partitions of a
TessAppend on its outer side by the keys of the table it built: the
planner (`nodes/join_planner.c`) writes the core's pruning descriptions
for `key = $p` and for a range over parameters of its own, and at
execution the join sets them from its keys. The planner also expects
the pruning in the join's cost, and under a gather offers an outer side
whose partitions the participants divide. The text is in
`docs/nodes.md` (TessHashJoin, "Pruning the outer side's partitions";
TessAppend, "Pruning while executing" and "A join's keys"), in
`docs/costs.md`, and in the comments of those files.

## Goals / Non-Goals

**Goals:**
- One place that says when partitions are pruned while a query runs,
  by what, and what EXPLAIN shows of it.
- EXPLAIN showing the same count in a parallel plan whichever
  participants pruned, and the core's lines in every format.
- Each statement tied to a test.

**Non-Goals:**
- Pruning while planning, by constants: the core's, before any node of
  Tessera exists.
- TessAppend's other work, how it reads and shares its children, and
  the hash join's other work: their own capabilities.
- The join's own cost terms over the partitions left: a change of the
  cost model, which needs a measured change.

## Decisions

### The count of a join's pruning from every participant

**Was:** `Subplans Removed by Join` showed the count the leader's node
found. Without the leader, the leader never pruned, and the line said 0
over a join that removed three partitions of four.

**Will be:** every participant that prunes adds the count and a 1 to two
counters of the node, summed over the participants as its batches are;
the line shows the first divided by the second. Every participant
prunes the same partitions, so the quotient is the count. A maximum
over the participants would need a call the runtime does not have: its
counters are sums.

### `Subplans Removed` in every format

**Was:** the node showed the line only when it removed a child.

**Will be:** in the text format only then, in the other formats always,
as the core's `Append` does, so that a tool reading JSON finds the same
keys on the node as on the core's `Append`.

### The expectation of a hash partitioning

**Was:** the planner bounded the inner key for a hash partitioning by
the steps of a btree one. The strategy number 1, "less than" in a btree
family, is equality in a hash family: an inner clause `k = 5` gave the
outer clause `k = 5`, and the planner expected one partition, while
`5 = k`, commuted to strategy 5, gave nothing. The statistics went
through the hash equality as a comparison and gave nothing either.

**Will be:** for a hash partitioning the planner reads no statistics
and takes an inner clause that sets the key equal to a constant, either
way round, as `outer key = constant`. Any other clause gives nothing.

### What we do not do

- The join's own terms, its probe, batches, Bloom filter and spilling,
  keep counting the rows of every outer partition, so the cost of a
  pruning join is higher than it will be. Counting only the partitions
  left would change which plans win and needs a measured change; the
  finding goes to the roadmap.
- `Subplans Removed by Join` keeps showing the count of the last table
  built: over rescans that build anew the counts may differ, and the
  core has no line of this kind to follow.
- The list of keys keeps its bound in rows with a key, not in distinct
  keys: a count of distinct keys would cost a set of them at the build.
- The pruning steps in the node's plan data do not pass through the
  core's fixing of plan references, and the node's partitions do not
  join the statement's prunable relations. Both matter for row marks
  and for data-modifying statements, which the node does not take.
