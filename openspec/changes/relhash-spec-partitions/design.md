## Context

The table lives in the crate `relhash`, and Tessera's C calls of
`tessera/table.h` are a thin layer over it in `tessera-capi`. The
first change of the move, `relhash-spec-table`, made the capability
`relhash` with the index, the chunks, the records and the calls of the
table, and gave `hash-table` the requirement "The C API over the
crate". This change moves the rest of what the crate does: partitions,
the Bloom filter, the marks of RIGHT and FULL joins, and what makes a
table damaged. Then `table-crate` sets its own requirements on top.

## Goals / Non-Goals

**Goals:**

- The promises of partitions, filters, marks and a damaged table in
  the crate's spec and its terms.
- One place for each rule: the C API points to the crate's spec.
- The same tests behind every promise.

**Non-Goals:**

- No new promise and no test changed. The crate's errors of two kinds,
  which take the place of "A damaged table", are `table-crate`'s.
- No rewrite of the design for a caller of the crate: that is a task
  of `table-crate` at its close.

## Decisions

### The text as the code is today

Four of the requirements that move are rewritten in the delta of
`table-crate`: appending rows by partition and splitting a chunk name
the count of keys, not their kinds; a find or insert by partition adds
to its mask of inserted rows; and the errors name the reserved bytes
and a gather refused late. They move as `table-crate` has them, and
their entries leave its delta, by the rule the maintainer chose for
the first change: the text in `main` names kinds of keys the crate does
not have.

### A damaged table, for a while

"Errors of a call" holds two things: what makes the bytes of a table
not a table, with the bound of a walk and the order of the checks,
which the crate does; and the status and SQLSTATE that Tessera gives
such an error, which the C layer does. The first moves to `relhash` as
"A damaged table"; the second stays, as "The status of an error".
When `table-crate` closes, its
"Errors of two kinds" takes the place of "A damaged table", by an entry
of its delta that removes it. Until then the rules of a damaged table
are in the crate's spec, written once.

### What stays with the C API

The crate leaves some things to its caller, and the C layer does them.
They stay in `hash-table`:

- `tess_table_partition`, the partition of a hash in C; the crate
  takes its partitions as a `Partitions` and has no such call.
- The mask of results of a check against a filter, filled whole; the
  crate narrows one mask, as a lookup does.
- The index a call checks when it is given one: an append by partition
  refuses keys of an index other than the table's, and a mark or a walk
  a record size other than the index's. The crate's calls over chunks
  take no index.
- The marks of a shared table changed by an atomic OR: the crate takes
  the caller's marks through `Marks`, and its `set` takes `&self`, so
  that marks shared by participants change by atomics of the caller.
- A participant that stops while it probes, and the word it marks for
  the last one. This is a step of a shared build, not a call of the
  table, and stays as "A participant that stops while it probes".

"The C API over the crate" names, for each C call over partitions,
filters and marks, the requirement of `relhash` it follows, and lists
the first three among what only the C API does, with a scenario of the
index a C call checks.

### The same tests

Every scenario keeps its tests, those of the C layer and the SQL suites
too: a scenario of `relhash` may be shown by a test that goes through
`tessera/table.h`. A scenario that mixes the crate's rule with one the
C layer keeps is split: the append by partition and the marks refused
for another index go to the C requirement, with the same tests; a
damaged header shown by the SQL suite goes to "The status of an error",
with its SQLSTATE.

### New names, not changed requirements

OpenSpec refuses a changed requirement that drops a scenario, since the
archive replaces the whole requirement. "Errors of a call" and "The
marks of RIGHT and FULL joins" lose scenarios to `relhash`, so they are
removed, and what stays of each is added under a name that says what it
holds. The close puts both where the removed ones stood.

### The designs

The sections of `hash-table/design.md` on partitions, the Bloom filter
and the marks move to `relhash/design.md` in a commit of their own,
word for word, and are then written in the crate's terms. What is
Tessera's goes back to `hash-table/design.md` as short sections that
point to `relhash`: how a join and a grouping use partitions, the
merge, how a level is planned, a partition too large for memory; when
a join wants a filter and where it keeps one; the last participant
that walks the records without a mark, and the one that stops while it
probes. The checks of a call stay with the C layer; their explanation
points to "A damaged table".

## Risks / Trade-offs

- [A promise is lost in the move] → each requirement and scenario is
  written again with the same tests, and the check of the specs finds
  every test it names.
- [The crate's spec names tests of Tessera] → a test through the C
  layer still shows the crate's promise; the crate's own tests for
  each promise are the work of `table-crate`.
- [The crate's comments still point to `hash-table`] → the comments
  of `lib.rs`, `header.rs` and `record.rs` name the spec of
  `hash-table` for rules now in `relhash`; `table-crate` rewrites the
  crate's documentation with links to its spec.

## What we do not do

- **Move "Merging groups read back".** Its kinds of state, a count, a
  sum, a minimum and a maximum, and the error of the int8 range are
  Tessera's; the crate merges by a rule the caller gives, a promise of
  `table-crate`.
- **Move the stop of a participant into the phases.** "The phases of a
  shared build" has its three scenarios; a fourth of another subject
  would make it harder to read.
