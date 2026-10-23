## Context

The table lives in the crate `relhash`, and Tessera's C calls of
`tessera/table.h` are a thin layer over it in `tessera-capi`. The spec
of `hash-table` was written before the crate, so it states the
table's promises through the C calls. The change `table-crate` plans
the move of these promises into the crate's own spec in two changes,
then sets its own requirements on top. This change is the first: the
index, chunks and references, a record, and the calls of the table.
The second moves partitions, the Bloom filter, the marks and what
makes a table damaged.

## Goals / Non-Goals

**Goals:**

- The promises of the table itself, in the crate's spec and its terms.
- One place for each rule: the C API points to the crate's spec.
- The same tests behind every promise.

**Non-Goals:**

- No new promise and no test changed. The crate's own promises, its
  errors of two kinds and its owning table among them, are
  `table-crate`'s.
- No rewrite of the design for a caller of the crate: that is a task
  of `table-crate` at its close.

## Decisions

### The text as the code is today

**Was:** the plan of `table-crate` moves each requirement as `main`
has it, and `table-crate` sets its own changes of four of them on top
afterwards, as entries that modify the moved requirements.

**Will be:** the four move as `table-crate` rewrote them: the index
with the magic `RELHASH` and version 2, a record whose keys are words,
appending checked by the count of keys, and a mask of inserted rows
that adds up. Their entries leave the delta of `table-crate`. The
maintainer chose this on 2026-10-23.

The text in `main` describes a table the crate no longer makes: a
magic, a version and kinds of keys the code does not have. Written in
the crate's terms it would name things that do not exist in the crate,
and each of the four would be written twice, once old and once new.
The price: these promises of `table-crate` reach the main specs before
that change closes. They are in the code and their tests already pass.

### What stays with the C API

The crate leaves some things to its caller, and Tessera's C layer does
them. They stay in `hash-table`, in one new requirement:

- How a key column fills its word: an `int4` widened with its sign, an
  `int8` as it is, so that the two match. The crate compares words.
- A key gathered as a Datum: the crate gives the word.
- The mask of results of a C lookup: the crate narrows one mask.
- A debug build of the entry points checks every chunk it is given.
- The limit of memory a node takes for a table.
- The atomic counters, spill words and marks beside a shared table,
  and the barrier between the phases of a build.

The rest of each C call is the crate's call, and the new requirement
says which requirement of `relhash` each call follows, instead of
repeating it.

### The same tests

Every scenario keeps its tests, those of the C layer and the SQL suites
too: a scenario of `relhash` may be shown by a test that goes through
`tessera/table.h`. The scenarios of what only the C API does go with
the C requirement: the match of `int4` and `int8` keys, the debug
check of every chunk, the limit of memory.

### The purpose of the crate's spec

The delta of `relhash` in `table-crate` already has a purpose for the
spec. It moves into this change, which makes the capability, and
leaves `table-crate`.

### The designs

The sections of `hash-table/design.md` that explain the table itself
move to `relhash/design.md` in a commit of their own, and are then
written in the crate's terms: the background, the problem and the
goals of the table, the whole in one picture, the index, chunks and
records, references, appending and linking, looking rows up, a key's
records together, one writer, several processes at once, and the loom
model of the link and the probe. `hash-table/design.md` keeps the keys
of each SQL type, values longer than a word, the limit of memory, the
checks and errors, partitions, the Bloom filter, the marks, the shared
build, the spill and the rounds, and points to `relhash` for the table.

## Risks / Trade-offs

- [A promise is lost in the move] → each requirement and scenario of
  the nine is written again with the same tests, and the check of the
  specs finds every test it names.
- [The crate's spec names tests of Tessera] → a test through the C
  layer still shows the crate's promise; the crate's own tests for
  each promise are the work of `table-crate`.

## What we do not do

- **Move the errors now.** "Errors of a call" names Tessera's status
  and SQLSTATE beside the rules of a damaged table; the second change
  moves what makes a table damaged, and `table-crate` replaces the rest
  by its errors of two kinds.
