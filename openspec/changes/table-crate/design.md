## Context

This design is about the move. Its folder goes when the change closes,
and what a reader of the crate needs is then written into the design
of `relhash` (task 7.1); nothing of this context goes there.

The move cuts these ties of the table to the rest of Tessera:

- `exclusive.rs`, the folds of aggregate states into a payload:
  `crate::decimal` and `crate::ops::ArithmeticError`.
- `order.rs`, the items of a sort: `crate::sort`.
- `keys.rs`, keys from columns: `tessera_core::ColumnReader` and
  `WordBlock`.
- `lanes.rs`, the prefetch hint: `crate::simd::prefetch`.
- Every file: `tessera_core::{RowMask, RowMaskView, ones}` and
  `anyhow`.

`header.rs`, `region.rs`, `record.rs`, `phases.rs` and
`shared_spill.rs` need nothing outside the table. The workspace builds
without LTO, so a small function of another crate is inlined only when
it is generic or marked `#[inline]`: the calls the crate gives
Tessera's loops are generic for that reason.

## Goals / Non-Goals

**Goals:**

- The table as a crate that a database engine outside Tessera can take
  as it is.
- Not one instruction more in a hot loop of Tessera.
- The promise "any bytes, a bounded call" shown by proofs and tests
  that cover every place the table trusts its own memory.

**Non-Goals:**

- See "What we do not do".

## Decisions of the maintainer

On 2026-10-10, in a discussion of the table and in the review of this
proposal:

- The crate lives in this workspace and is named `relhash`. "Rel"
  stands for the relative references that replace addresses, and for
  the relational engines the table is built for. Names that begin with
  `tessera-` were refused: on crates.io about 120 crates carry that
  word, a family of them belongs to a user interface framework, and
  such a name would suggest that the crate needs Tessera.
- Its API is by batches only, and the batches are the crate's own types,
  free of Tessera.
- The Bloom filter is a module of the crate.
- The marks of RIGHT and FULL joins go into the crate: a building block
  of the hash table of any database engine.
- The phases of a shared build stay in Tessera.
- Kani is added to the checks.
- Fuzzing runs briefly on every pull request, and for one to two hours
  before a release of the crate.
- The crate's documentation recommends a build with LTO.
- The promise over any bytes holds for a table that no other process or
  thread writes while a call runs, over blocks aligned to 8 and
  initialized.
- The crate has no walk over records of its own: the items of a sort
  are made through the scan and the cursor that reads a record.
- 32-bit targets are of no interest: the crate builds for 64-bit ones
  only.
- The spec of the crate comes in changes of its own: two move the
  requirements of `hash-table` that describe the table itself into the
  capability `relhash`, nine each; this change adds the new promises.
- The crate's documentation does not repeat its spec: it links to it.
- The table that owns its memory stays in the crate, behind a feature
  `alloc` on by default, outside the promise of no memory. Nothing of
  Tessera's product uses it, but it is the crate's way in without
  unsafe code, and the table of its tests and benchmarks.
- The work goes as small pull requests into a branch `relhash`, each
  one the maintainer reads, and the branch goes into `main` whole at
  the end.

## Decisions

### What goes into the crate

**Was:** one module of `tessera-kernels`, with the folds of aggregate
states, the items of a sort, keys from Tessera's columns, the phases of
a shared build and the words of a spill among the parts of the table.

**Will be:** the line between the two is whether a part knows SQL or
Tessera's executor.

- Into the crate: `header.rs`, `region.rs`, `record.rs`, `batch.rs`,
  `local.rs`, `bloom.rs`, `marks.rs`, `lanes.rs`, `mod.rs`; of
  `keys.rs`, the trait of a source of keys; of `exclusive.rs`, find or
  insert, its form by partition, grouped link, scan, regrow and clearing
  a key; of `loom.rs`, the models of the table, the filter and the
  marks.
- Into the crate from `tessera-core`: `row_mask.rs`, `bits.rs` and
  `bitmap.rs` (see "The masks of rows").
- Stay in the kernels: the folds, sums and extremes of `exclusive.rs`
  and the kinds of state its merge knows; `order.rs`; of `keys.rs`, the
  reading of keys from Tessera's columns; `phases.rs`;
  `shared_spill.rs`; of `loom.rs`, the models of the phases, the rounds
  and the words of a spill.

Refused: the folds in the crate, since they carry PostgreSQL's numeric
and its int8 error; the phases in the crate, since they are the steps
of a join over PostgreSQL's barrier, with steps of a spill.

### The masks of rows

**Was:** `RowMask`, `RowMaskView` and `ones` live in `tessera-core`,
with the checks of `bitmap.rs` that its columns use too; every kernel
uses them.

**Will be:** they move into the crate, which owns its batch types.
`tessera-core` exports them again under the same names, so no other
code changes its imports. A view of a mask keeps both its forms, words
and a window of bytes, since the columns of `tessera-core` read their
NULL bits through the second. Reading a word past a mask's words still
panics, as it does now: a check there costs 4 to 9 instructions a word
in the kernels' loops. It is a call over a mask, not over a table's
blocks, so the promise over any bytes does not reach it.

Refused: a mask of the crate's own beside the old one, two copies of
one type; a third crate for the masks alone, one more crate to publish
and keep for about 450 lines. The cost: `tessera-core` now depends on
`relhash`, which a reader may find upside down.

### Errors of two kinds

**Was:** `anyhow::Error` with a text. The files that move build one in
about a hundred places. `tessera-capi` passes any of them on as
`TESS_ERROR_INVALID_ARGUMENT` with its text.

**Will be:** an `Error` of two kinds, a wrong call and a damaged table,
each a closed list of the rules a call checks; the spec lists them. The
kind follows where the broken value came from: an argument makes a wrong
call, a byte read from the table's blocks a damaged table. The error
carries a number or two that explain the case, no text and no
allocation; its `Display` writes the message the C side passes on, so
what PostgreSQL shows stays as it is. One check may give either kind: a
reference is checked by one function whether the caller passed it or the
call read it from a bucket or a record, and the call site, which knows
where it came from, chooses the kind. The one error today that names no
rule, a header the fast check and the full one judge apart, becomes a
debug assertion: the two checks are one rule written twice. A merge
returns the error of the caller's rule beside the crate's own. Tests
compare kinds and rules, not words.

The error takes at most 8 bytes, and a compile-time assertion keeps it
so. Then a `Result<usize, Error>` stays 16 bytes, as with
`anyhow::Error`, a single pointer, and the hot calls keep returning it
in two registers. A larger error would be returned through memory.

Refused: keeping `anyhow` and looking for words, which allocates and
does not tell the checks apart.

### The calls for a caller's payloads

**Was:** the folds and the items of a sort use the table's inner parts:
the access to a record, the place of a payload, the memory under it.

**Will be:** two calls, each generic and inlined, so that the loops of
Tessera compile as they do now:

- **Cursors by reference.** A cursor is made once a call and keeps
  what the table's access keeps. One reads the record a reference
  names. The one writer's other cursor gives the payload a reference
  names, to change; it takes itself by `&mut`, so two payloads are
  never open at once, even for two rows of one record. For the sums
  that keep the payloads of a few groups at hand, it checks a reference
  now and opens its payload later. The items of a sort take their
  references from the scan and their keys through the reading cursor.
- **A merge by a rule.** The loop that merges groups read back from
  disk today, with the rule given as a closure.

Refused: making the inner access public and unsafe, which would put all
of it into what must be proven; keeping the folds in the crate, see
above; a walk over records of its own, which the scan and the cursor
already give. If a loop of Tessera compiles longer through these calls,
other forms are tried as "Stop and ask" of `CONTRIBUTING.md` says, each
a hypothesis written down first. When none keeps the loop as short,
the cost in speed against the crate's boundary is a trade-off no rule
settles, and the maintainer decides it.

### The owning table

**Was:** `LocalTable` owns its index and chunks, allocates them, adds
chunks as they fill and makes a larger index when the records outgrow
it. The C nodes never use it; the tests of the table and of the C entry
points and the benchmarks of counters do.

**Will be:** it stays in the crate as its table that owns its memory,
behind a feature `alloc` on by default. It is the only way to use the
crate without unsafe code, so the README's example uses it. It is
outside the promise of no memory: allocating is its job. A build
without the feature leaves it out, and has nothing that allocates.

Refused: moving it into the tests, which would leave a caller from
outside only the unsafe API; dropping it, which would make every test
and benchmark manage raw memory.

### The loom models

**Was:** `loom.rs` models the table, the filter, the marks, the phases
of a shared build, the words of a spill and the rounds. The models of
the phases and the rounds run the table's own code over the model of
its memory.

**Will be:** the crate keeps the models of the table, the filter and
the marks. The kernels keep the others. For them the crate gives, only
when built with `--cfg loom`, a model table: append, size, link and
probe over loom's atomics. A normal build has no such API.

Refused: modelling the phases over a stand-in table. It would lose the
negative test where linking before the index exists breaks the table.

### The prefetch hint

**Was:** the probe of a batch asks the processor to fetch the next
memory through `crate::simd::prefetch` on AArch64, and does nothing on
other processors, x86-64 among them.

**Will be:** the crate has its own copy of the same hint, the same
instruction on AArch64, and nothing elsewhere. That x86-64 lacks it is
already planned by the roadmap's `simd-primitives-avx2`, among its
commits; once the table is in the crate, that commit is written there.

### 64-bit targets only

The crate's arithmetic of offsets assumes a 64-bit `usize`: the bytes of
the buckets, `4 * nbuckets` with up to 2^31 buckets, would wrap on a
32-bit target and pass the check of the index. Tessera runs on 64-bit
targets only, so the crate refuses to build on others with a
`compile_error!` that says why, instead of checking every product.

### What proves the promise over any bytes

The spec's "Any bytes, a bounded call" is the promise. It holds for a
table no other process or thread writes while the call runs. A shared
table that is being built breaks that condition on purpose: a call
checks a reference against the length of its chunk, not its used mark,
so as never to race with the chunk's writer, and a damaged reference
can point at bytes being written. For a shared table the spec promises
only what "Calls at the same time" says, over a table that is not
damaged; loom checks it.

Seven means show the promise, each covering what the others cannot:

1. **The trust points.** A trust point is a place where a call reads
   the index or a chunk. All such reads go through `Region`, so the
   list is finite. Each call site carries a mark, `// trust:` and a
   name, and a script fails when a read has no mark or a mark is not in
   the list. The list goes into the design of `relhash`: for each
   point, whether its value decides an address, the length of a loop,
   or only data; its check; its error; its proof; its row of the
   matrix.
2. **Kani.** A proof for all values, not for samples: a header that
   passes its checks puts every bucket inside the index; a reference
   that passes puts its record inside its chunk; a walk ends within its
   bound; a used mark that passes ends a record; a partition, a word of
   the filter and a word of the marks lie inside their arrays. The same
   runs show that these functions cannot panic. Whole calls are too
   long for Kani to unroll; the next two means cover them.
3. **The matrix of damage.** proptest builds valid tables: duplicate
   and NULL keys, several chunks, after a regrow and a split, with a
   filter and marks. One damage from a closed list goes into each,
   every field at and around its limits. Every call runs over it, the
   probe both row by row and by whole words, and then more calls after
   the error. A test checks the kind and the rule of the error, and
   guard words around every block that must not change. A few cases
   also run under Miri.
4. **Fuzzing.** cargo-fuzz over the same builder, with the input chosen
   by the fuzzer, under ASan. A pull request runs each target for a
   minute; a run of one to two hours goes before a release of the
   crate, on request. A finding becomes a case of the matrix.
5. **Mutants.** Every mutant of the crate, not only those of changed
   lines, and none survives but the equivalent ones, each listed with
   its reason. Since the tests compare rules, a removed check is seen.
6. **Miri** over all the tests of the crate.
7. **Tests of the rest of the spec:** every call under an allocator
   that counts; one sequence of calls run twice over two copies; a
   handle sent to another thread that does not compile.

### The spec of the crate, in three changes

**Was:** the spec of `hash-table` holds the table and what Tessera keeps
beside it, in the terms of the C calls.

**Will be:** once the code is in the crate, two changes make the
capability `relhash` and move into it the requirements that describe
the table itself, written again in the terms of the crate, with the
same tests. Each moves nine, under the limit of ten a change:

- `relhash-spec-table`: the index of a table, chunks and references, a
  record; appending rows, linking records, looking rows up, the records
  of a key together, calls of one writer, calls at the same time;
- `relhash-spec-partitions`: partitions of a table, appending rows by
  partition, splitting a chunk by partition, groups by partition; a
  Bloom filter of the keys, its size, a shared filter, a filter filled
  together; the marks of RIGHT and FULL joins. Of "Errors of a call",
  what makes a table damaged moves with it.

`hash-table` keeps what Tessera builds around the table: the SQL types
of a key; the status and SQLSTATE of an error; the kinds of state of a
merge of groups; the counters, phases and rounds of a shared build; the
words of a spill, its split, its eviction, its records and files; a
partition read back that splits; the partitions and chunks of a level.
It gets one new requirement: each call of `tessera/table.h` does what
the crate's call does, pointing to the requirement of `relhash`, so
that no rule is written twice. The designs split the same way.

Neither change adds a promise or changes a test. Both close before this
one. Then this change's own requirements are set on top of the moved
ones: "Errors of two kinds" takes the place of the part of "Errors of a
call" that moved, by an entry that removes it, so that the rules of a
damaged table are written once.

### The branch and its pull requests

The work is long, and the maintainer reads every step. So it goes into
a branch `relhash` from `main`, a pull request for each task of this
change and of the two that move the spec, one to three commits each.
CI runs on every pull request, whatever its base. The moves of files
are split by groups of files, each a `git mv` with the edits of paths,
read through `git diff -M` and `cargo ab moved`.

- A move that takes a file named by the spec or the design of
  `hash-table` edits those paths in the same commit, so that
  `check-specs.sh` passes at every step of the branch.
- When `main` moves, it is merged into the branch, so its history is
  never rewritten. Work on the table in `main`, such as the prefetch
  hint of `simd-primitives-avx2`, waits for the branch, or the branch
  takes it right after.
- At the end one pull request brings the branch into `main`. When
  `main` has not moved since its last merge into the branch, that merge
  is fast-forward, and every small commit stays as it was read.

### The documentation of the crate

**Was:** the doc comments of the table restate its spec and its design:
the module of the header lists its fields by offset, and the module of
the table retells the design in about 50 lines.

**Will be:** each text has one reader and one job.

- The spec says what is promised: rules, layouts, limits, the rules of
  errors, the caller's promises, each with its test.
- The design says how the crate is built and why.
- The documentation is a map of the API for a caller: for each item,
  what it is for in a sentence or two, its parameters, who owns what
  and for how long, its `# Safety` duties in the terms of the code, the
  kind of error it returns, and examples. For a rule, a layout or a
  limit it links to the requirement of the spec, by an absolute link to
  the repository, so that the link works on docs.rs too.
- The crate's page is its README, taken into the documentation with
  `include_str!`, so that crates.io and docs.rs show one text: what the
  crate is for, an example without Tessera, the caller's promises in a
  line with a link, the advice to build with LTO and to use a hash with
  a secret seed for keys from outside, and links to the spec and the
  design.
- `#![deny(missing_docs)]` in the crate; CI builds the documentation
  with every warning an error, and runs its examples.

The links need the repository to be public when the crate is published.

### The contract of the crate

The crate is a new part, so it gets an architecture contract, as
`CONTRIBUTING.md` asks. Until this change closes the contract lives in
its folder, `contract.md` beside this design, and the checks read it
there; at the close it moves to `openspec/specs/relhash/`. Its rules
keep the crate free of Tessera and of SQL, its memory behind one layer,
its unsafe code where the proofs reach, every reference checked by the
crate alone, the masks of rows its own, its calls free of allocation,
its API small and its cost to Tessera nil; most are `pending` until the
task that writes their check. Its forecast names three likely changes:
a prefetch hint for x86-64, keys of more kinds, removing records. A
probe of each runs once the crate exists, on branches that are never
merged, and its count of what it touched goes into this design.

### Speed

Every commit is one of two sorts.

- **A move.** The machine code of every benchmark program and of the
  kernels' module is the same as the base's: `tessera-bench --base REF
  --disasm` and `--disasm --module`, with `cargo ab moved` for a file
  that is cut. Same code, same speed; no timing is needed.
- **A change**: the errors, the masks, the cursors, the merge. First the
  machine code, where no loop may grow without a reason written down;
  then the counters, run by the maintainer, and `cargo ab measure` on
  the families of joins, groupings and sorts.

Tessera builds without LTO, and all of this is measured without it.

### A fifth surface of a spec

**Was:** a spec names four surfaces: the C API, what SQL sees, formats,
and the entry points of the kernels.

**Will be:** a fifth, the public API of a crate of the workspace meant
for use outside Tessera, one with `publish = true` in its `Cargo.toml`,
a fact a reader can check; a change of such an API needs an OpenSpec
change, as one of the C API does. Then the spec of `relhash` can
promise what the crate's callers rely on. The same edit of
`CONTRIBUTING.md` says, beside the rule that a rule is written once in
the spec, that a crate's documentation links to its spec and does not
repeat it. All of it lands with this proposal, since approving it
approves them.

## Measurement plan

- **What:** the machine code and the instructions of the table's calls,
  and the time of SQL queries that use the table.
- **Tools:** `tessera-bench --base REF --disasm` on every program and
  with `--module`, every commit; `tessera-bench --base REF` with
  counters on `table_int32`, `table_large` and the programs of
  aggregates, for a commit that changes code; `cargo ab measure` on
  the join, group and sort families of `bench/pg`.
- **Thresholds:** those of `docs/measuring.md`. A move must show the
  same machine code. A change fails at more than 1 % instructions, or
  at cycles past their limits. A confirmed excess starts the search of
  "Stop and ask"; one the search does not remove goes to the
  maintainer.

## What we do not do

- **A call for one key, typed keys and values.** For one key at a time
  hashbrown is faster and smaller. A typed layer may come later, over
  the API of batches.
- **Removing records.** Records are only appended. A removal by one
  writer is possible later; by several at once it is a hard problem of
  freeing memory.
- **The promise over damaged bytes in a shared table being built.** It
  would need an atomic read of every byte of a record, at a cost on
  every probe.
- **`no_std`.** A call allocates nothing, so the core could do without
  the standard library, but nobody needs that now. The feature `alloc`
  only leaves the owning table out.
- **32-bit targets.**
- **A crate for spilling.** `tessera-spill` writes Tessera's columns,
  and the files are written by C. Taken up again once the table lives
  apart.
- **A defense against chosen collisions.** The caller gives the hash.
  The README says to use a hash with a secret seed for keys from
  outside.
- **Checksums of records.** Bytes that are damaged but still look like
  a table give a wrong answer without an error. Only a checksum read
  with every record would catch them, at a cost on every probe.
- **A prefetch hint on x86-64** in this change: `simd-primitives-avx2`
  writes it.
- **A full formal proof** with Verus or Creusot. The code would have to
  be written again in their subset. The risk lies in the unsafe code
  and the protocol, which Kani and loom check.
- **Publishing on crates.io.** The crate is packed by a dry run in CI;
  publishing is decided later.

## Risks / Trade-offs

- [A loop of Tessera compiles longer through the cursors or the merge]
  → the machine code of every commit; a search of other forms, then
  the maintainer.
- [The error grows past 8 bytes] → a compile-time assertion.
- [`tessera-core` depends on the table's crate] → written down above.
- [Kani cannot unroll a whole call] → it proves the checks and their
  arithmetic; the matrix and the fuzzer cover whole calls.
- [Fuzzing takes CI time] → a minute a target on a pull request; the
  long run only on request.
- [The model table is a second form of the API] → only under
  `--cfg loom`.
- [This change's spec is written before the requirements it builds on
  are moved] → a task sets it on top of the moved ones before its
  checks start.
- [The branch lives long while `main` moves] → `main` is merged into
  it as it moves; work on the table in `main` waits for the branch.

## Migration Plan

Nothing for the C nodes: `include/tessera/table.h` stays as it is. The
Rust code of the workspace keeps its imports through the exports of
`tessera-core` and `tessera-kernels`. A revert of the merge of the
branch undoes it.
