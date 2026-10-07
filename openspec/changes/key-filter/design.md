## Context

A hash join builds a Bloom filter of its table's keys when most of its
probe rows find no pair (capability `hash-table` describes the filter).
It may hand the filter to its outer child through `set_key_filter` of
the node contract: TessFilter checks its rows against it before its
row-wise clauses, and TessAppend forwards it to its partitions. The
text is in `docs/nodes.md` (the filter's and the join's sections),
`docs/node.md` and `docs/runtime.md` (the guides of the contract), and
the comments of `node.h`, `runtime_input.h`, `nodes/filter.c`,
`nodes/append.c` and `nodes/hashjoin.c`.

## Goals / Non-Goals

**Goals:**
- One place that says what a key filter is, who hands it down, who
  takes it, what the child removes, and what EXPLAIN shows.
- The same line of EXPLAIN counting the same rows in both nodes.
- Each statement tied to a test.

**Non-Goals:**
- The filter itself, its words and its calls: capability `hash-table`.
- When a join builds a filter, and the rest of the join and of
  TessFilter: their own capabilities.
- Whether a line of EXPLAIN is a total over loops or a mean: the join's
  lines are totals, TessFilter's per loop as the core's; the spec says
  which, and changing either is a question for those capabilities.

## Decisions

### A line of its own for a NULL key

**Was:** TessFilter counted every row its key check dropped as removed
by the Bloom filter, those with a NULL key among them.

**Will be:** `Rows Removed by Bloom Filter` counts only the rows with a
key that the filter rejected, as the join's line does, and
`Rows Removed by NULL Key` the others. Counting NULL keys in the join
instead is not possible: a LEFT or ANTI join returns such rows.

### The contract as parents keep it

**Was:** `node.h` promised the child that everything of the filter
stays valid until the parent takes it back.

**Will be:** the struct and its arrays are valid during the call only,
and a child copies what it keeps; the words stay valid until taken
back. Both parents already hand the struct or the arrays from their
stack, and the only child already copies them, so no code changes.

### What we do not do

- The rows a TessFilter removes skip its later clauses, so an error such
  a clause would raise on them is not raised. PostgreSQL does not fix
  the order in which clauses run either; the spec says so.
- The join's lines stay totals over its loops.
