# key-filter: how it is built

This page is being written; the text below moved from the guide of the
nodes.

## TessFilter

A hash join above may hand the node its Bloom filter
(`set_key_filter`, [node.md](../../../docs/node.md)) when the node has a row-wise
clause and the join's keys are columns of the child: the node then
hashes the keys of the rows its batch clauses kept, as the join does,
and removes the rows the filter rejects before its first row-wise
clause, a shared filter once it reads it ready.

## TessHashJoin

An inner or
semi join, which drops a row without a pair and whose keys are all words
(the child has a hashed key's value, not its hash), then hands the filter to
its outer child (`tess_input_set_key_filter`): a TessFilter with row-wise
clauses takes it and removes the rows it rejects before those clauses
run, and the join checks no more; it takes the filter back before the
table goes. A left or anti join returns those rows and keeps the filter.

## TessAppend

A hash join above hands its Bloom filter down (`set_key_filter`, TessHashJoin),
which a child with row-wise clauses checks before them. The node hands
it to every child it reads, in the child's own columns (the child's
layout maps the node's columns to its batch's), and takes it only when
every child does: the join checks no more rows once a node below took
it, so the children that took it give it back when one does not.
Before, the filter stopped at the node, and a partitioned outer side
checked its row-wise clauses on every row. `test/sql/join.sql` hands it
to two partitions, the second with its columns in another order and the
join key apart from the partition key; mutations fail it: the node's
columns handed down as they are, no hand-down.

## Files

- `include/tessera/node.h`

## Tests

- `test/sql/join.sql`
