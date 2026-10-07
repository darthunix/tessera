## Context

The capability `hash-table` has eleven requirements about the table
itself. `docs/table.md` still holds what other parts build on the table:
partitions for spilling, the Bloom filter, the items of a sort, the
aggregate states of a grouping, and the phases of a shared build with
its rounds. The marks of RIGHT and FULL joins are described only in the
comments of `table.h` and, from the join's side, in `docs/nodes.md`.

The code is the module `table` of `tessera-kernels` (`batch.rs`,
`exclusive.rs`, `bloom.rs`, `marks.rs`), its C entry points in
`tessera-capi`, and the C API in `include/tessera/table.h`.

## Goals / Non-Goals

**Goals:**
- One place that says what the partitions, the filter and the marks
  are, in local and in shared memory, and what each of their calls
  promises.
- Each statement tied to a test by name.
- The code made to keep the promises that it breaks today, each
  correction with the test that shows it.

**Non-Goals:**
- How several participants agree: the phases of a shared build and of
  its rounds, the shared words of a spill. They are the next change of
  the same capability.
- The aggregate states and the calls that fold rows into them, which
  belong to the grouping's capability, and the items of a sort, which
  belong to the sort's.
- What the join and the grouping decide: when they spill, how many
  partitions they make, when they want a filter.
- Speed. No loop changes; the corrections add a check where a call
  starts.

## Decisions

### Two changes for the second half

**Was:** one entry of the roadmap for everything other parts build on
the table, about forty calls.

**Will be:** this change takes what lies beside the table and has a
format of its own: partitions, the filter, the marks. The next one takes
how participants agree: the phases, the shared words of a spill, the
rounds. Each stays within ten requirements.

### The index is checked when there is one

**Was:** an append by partition, a mark and the walk of the marks take
the record's size from their arguments and never read the index, even
when the table has one.

**Will be:** as an append does, they check the index when the table has
one and refuse keys, a payload or a record size other than its. A mark
of the wrong size writes past the caller's words, so this check guards
memory, not only a mistake. Without an index nothing changes: a join
may append by partition and walk its marks with only the chunks.

### The model of several participants

**Was:** the loom model built the shared filter, but filling a filter
together and marking together ran only in the SQL suites, where the
order of the participants' steps is left to chance.

**Will be:** the model runs both through the code of the kernels over
words of loom atomics: the filter's additions through a trait the real
atomic words implement too, the marks through the trait `Marks` they
already have. A negative test of each shows that a plain read and write
in place of the atomic OR loses bits.

### What we do not do

- The words of a filter and the masks of a probe are not checked for
  alignment or overlap: the C types already require both, as for every
  other array of the C API.
- A merge that fails after its checks is not rolled back. The node
  raises the error at once, as for every failed call.
- `tess_bloom_shared_add` keeps its name, although it takes a filter
  without a state word. The spec says so.
- A participant of a shared RIGHT or FULL join that stops before its
  outer side ends does not stop the last one from returning the inner
  rows without a mark. It is a question of the phases, for the next
  change.
