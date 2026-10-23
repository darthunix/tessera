## Why

The hash table is now the crate `relhash`, but what it promises is
still written in the spec of `hash-table`, in the terms of the C calls
of `tessera/table.h`. A caller of the crate reads there of
`tess_table_append`, of a `TessTableRef` and of a mask of found rows,
and finds none of the calls it makes: `TableMut::create`, a
`ChunkWriter`, a lookup that narrows its `Matches`. Nine requirements
of `hash-table` describe the table itself:

- The index of a table; Chunks and references; A record;
- Appending rows; Linking records; Looking rows up;
- The records of a key together; Calls of one writer; Calls at the
  same time.

Four of them are behind the code. The change `table-crate`, which made
the crate, rewrote them in its own delta, which reaches the main specs
only when that change closes: the spec in `main` still says that the
magic is `TESSTABL` and the version 1, that a key has a kind, and that
a find or insert clears its mask of inserted rows. The crate writes
`RELHASH` and a zero, version 2, keys as words, and a mask that adds
up over the calls of a batch.

The design of `table-crate` moves the requirements of the table into
the crate's spec in two changes of nine requirements each, before it
closes. This is the first.

## What Changes

- A new capability, `relhash`, with the nine requirements above. Each
  is written in the crate's terms, its calls, types and constants, and
  keeps its promises and its tests.
- The four that `table-crate` rewrote move as they stand in its delta,
  as the code is today, not as `main` has them; the maintainer chose
  this on 2026-10-23. Their entries leave the delta of `table-crate`.
- `hash-table` loses the nine and gets one requirement, "The C API over
  the crate": each call of `tessera/table.h` does what the crate's
  calls do, as the named requirement of `relhash` says, so that no rule
  is written twice. What only the C API does stays in it, with its
  scenarios: the key columns of `int4` and `int8`, a key as a Datum,
  the masks of results, the check of every chunk by a debug build, the
  limit of memory, and the atomic words Tessera keeps beside a shared
  table.
- The design splits the same way. The sections of
  `hash-table/design.md` that explain the table itself move to a new
  `relhash/design.md`, in the crate's terms; `hash-table` keeps what
  Tessera builds around the table and points to `relhash`.
- No code and no test changes.

## Capabilities

### New Capabilities

- `relhash`: the hash table of the crate `relhash`, its format and its
  calls. This change makes the capability with the requirements of the
  table itself; `table-crate` adds the crate's own promises on top.

### Modified Capabilities

- `hash-table`: nine requirements leave for `relhash`; "The C API over
  the crate" comes in.

## Impact

- `openspec/specs/hash-table/spec.md` and `design.md`.
- `openspec/specs/relhash/spec.md` and `design.md`, new.
- `openspec/changes/table-crate/`: its delta of `hash-table` loses the
  four requirements that move here, its delta of `relhash` its purpose,
  which comes with this change, and its tasks and design say so.
- No public header, setting, SQL suite or code. No entry of
  `openspec/roadmap.md`: the work is part of `table-crate`.
