## Context

TessHashJoin builds a table of its inner side and probes it with
batches of the outer side. RIGHT and FULL keep the inner side too: the
probe marks each record that finds a pair after the join's other
clauses, and after the outer side the node walks the table and returns
the records without a mark, with NULL outer columns. The marks are kept
by every table: one in memory, a shared one, each partition of a spill.

The core's `JOIN_RIGHT_ANTI` has the same sides: the inner side is the
one kept, and a row of it is returned when no outer row matches it. Its
target holds only inner columns.

## Goals / Non-Goals

**Goals:** the core's right anti join done by TessHashJoin with the
results of the core; the plan chosen by cost against the core's right
anti join and against TessHashJoin's own anti join of the other
direction.

**Non-Goals:** a stop of the probe once every record is marked; a key
filter handed to the probe side's scan where the RIGHT join has none.

## Decisions

### RIGHT without its pairs

At its start the node reads `JOIN_RIGHT_ANTI` as RIGHT: an inner join
that keeps the inner side. The probe makes the pairs and marks their
records as RIGHT does, and returns none of them; the tail returns the
unmarked ones. Every table, shared or spilled, keeps its marks
already, so nothing else changes. Pairs of several records a key are
not copied into compact batches, which only serve the pairs returned.

Refused: an anti mode of its own in the probe, that marks without
making the pairs. The pairs are only offsets of records, and the mark
needs them; a mode of its own would repeat the rounds and the spill
paths for no work saved.

### The cost

`join_cost` counts the pairs of an inner join of the two sides, as the
core does for its right anti join (`approx_tuple_count`, which is not
exported: the selectivity of the hash clauses with a join of the two
sides), and charges a pair the probe and the mark, and a returned record
the gather of its columns. Nothing is returned during the probe.

### The probe side, as RIGHT's

An outer row without a pair leaves a right anti join, as it leaves a
RIGHT one. So the join hands its Bloom filter to the probe side's scan
and prunes a partitioned probe side by its keys under the same
conditions as RIGHT; the specs `key-filter` and `partition-pruning`
name it with RIGHT.

### Under a Gather

As for RIGHT, a table in each participant would return each record
without a pair once a participant; only the shared table is offered.

## Risks / Trade-offs

- The core costs its right anti join with nominal constants, TessHashJoin
  with its measured ones; a query where both are close may change its
  plan between the two right anti joins, with the same result.

## Measurement

`cargo tpch` Q22, base and change twice in turn, serial and with two
workers, Q21 and Q4 (EXISTS and NOT EXISTS) as controls; `cargo ab
measure` over the join family with the new case `right_anti`.
