## Why

The spec of `relhash` holds the index, the chunks, the records and the
calls of the table. What else the crate does is still written in the
spec of `hash-table`, in the terms of the C calls: the partitions of a
table that spills, the Bloom filter of its keys, the marks of a RIGHT
or FULL join, and what makes a table damaged. A caller of the crate
reads there of `tess_table_split` and of a mask of results, and finds
none of the calls it makes: `ChunkWriter::split`, `Partitions`,
`bloom::probe`, a `Marks` of its own. Nine requirements of
`hash-table` describe these, and a part of a tenth:

- Partitions of a table; Appending rows by partition; Splitting a
  chunk by partition; Groups by partition;
- A Bloom filter of the keys; The size of a Bloom filter; A shared
  Bloom filter; A filter filled together;
- The marks of RIGHT and FULL joins;
- of Errors of a call, what makes a table damaged.

Four of them are behind the code. The change `table-crate`, which made
the crate, rewrote three of the partitions and the errors in its own
delta, which reaches the main specs only when that change closes: the
spec in `main` still says that a key has a kind, and that a find or
insert by partition fills its mask of inserted rows exactly.

The design of `table-crate` moves the requirements of the table into
the crate's spec in two changes before it closes. This is the second.

## What Changes

- `relhash` gets ten requirements: the nine above, and "A damaged
  table", the part of "Errors of a call" that the crate keeps. Each is
  written in the crate's terms, its calls, types and constants, and
  keeps its promises and its tests.
- The four that `table-crate` rewrote move as they stand in its delta,
  as the code is today, by the rule the maintainer chose for the first
  change. Their entries leave the delta of `table-crate`.
- `hash-table` loses the nine and "Errors of a call". Two take their
  place with what Tessera keeps: "A participant that stops while it
  probes", of the marks, and "The status of an error", the status and
  the SQLSTATE of an error and the checks of the C arguments. "The C
  API over the crate" names the requirement of `relhash` that each
  call over partitions, filters and marks follows, and keeps what only
  the C API does: the partition of a hash in C, a mask of results
  filled whole, the index a call checks when it is given one.
- The design splits the same way. The parts of `hash-table/design.md`
  that explain partitions, the filter and the marks themselves move to
  `relhash/design.md`, in the crate's terms; `hash-table` keeps how a
  join and a grouping use them, and points to `relhash`.
- No code and no test changes.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

- `relhash`: the requirements of partitions, the Bloom filter, the
  marks and a damaged table come in.
- `hash-table`: nine requirements and "Errors of a call" leave; "A
  participant that stops while it probes" and "The status of an error"
  come in; "The C API over the crate" changes.

## Impact

- `openspec/specs/hash-table/spec.md` and `design.md`.
- `openspec/specs/relhash/spec.md` and `design.md`.
- `openspec/changes/table-crate/`: its delta of `hash-table` loses the
  four requirements that move here, and its tasks and design say so.
- No public header, setting, SQL suite or code. No entry of
  `openspec/roadmap.md`: the work is part of `table-crate`.
