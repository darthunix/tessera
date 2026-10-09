## Context

A table that spills splits into partitions, level by level: a level's
partitions take the bits of the hash after those the levels above took.
Before a node fills a level it plans it: how many partitions, and how
long their chunks. The rule is one, in
`crates/tessera-spill/src/plan.rs` behind `tess_spill_partitions` and
`tess_spill_chunk_len`; the grouping, the join and the shared table give
it their own parameters.

## Goals / Non-Goals

**Goals:**
- The rule of a level's plan as requirements of `hash-table`, every
  statement tied to a test.
- Arguments no sound node gives refused, as the table's other calls
  refuse them.

**Non-Goals:**
- The parameters each node gives, and when it plans a level: the
  nodes' capabilities.

## Decisions

### The bits of the hash

**Was:** the count doubled only while the doubled count's bits stayed
below 32 after the shift, but the least count was taken whatever the
shift: at a shift of 31, four partitions took bits past the hash.

**Will be:** a shift past which the least partitions do not fit in the
32 bits is refused. A node never gives one: a partition splits into a
level below only when two bits are left.

### The least chunk

**Was:** the length was rounded down to a multiple of 8 after the
least bound, so a least of 4100 gave 4096, and a least of 0 could give
0.

**Will be:** a least chunk below 8, a chunk's header, or not a multiple
of 8 is refused; the nodes give 8 KiB or four records, multiples of 8.

### Expected bytes

**Was:** a number below zero or not a number counted as nothing.

**Will be:** refused, as the weights of the rule of eviction are.
