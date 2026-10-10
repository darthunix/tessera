## Why

The hash table is the part the rest of Tessera is built around. A join
and a grouping keep their rows in it, a sort keeps its rows as its
records, and it works the same in one process and in shared memory. It
is also the part most worth using outside Tessera: records that never
move, no address inside, a build by several processes without a lock,
and lookups a batch at a time. On 2026-10-10 the maintainer decided to
make it a crate of its own, `relhash`: small, deterministic, checked as
fully as the tools allow, free of Tessera, and no slower. Today it is
none of these:

- It is a module of `tessera-kernels` and reaches into the rest of that
  crate and into `tessera-core`: the SQL decimals and the error of an
  int8 overflow for the aggregate states kept in a payload, the sort's
  encoder for the items of a sort, Tessera's column readers for keys,
  the SIMD module for one prefetch hint, and `tessera-core` for the
  masks of rows.
- Its errors are text. A test that damages a table can check only that
  a call failed, or look for a word of the message. It cannot tell
  which check refused the bytes, so it cannot show that each check is
  needed.
- The promise that wrong bytes give an error, never a crash or an
  endless loop, rests on tests written by hand, each kind of damage
  against one or two calls: 18 damaged fields of the header against
  attaching, four bad references and a loop against a probe, two used
  marks against a scan and a link. The other calls go through the same
  code, but no test shows it. About 200 lines of unsafe code rest on
  reasoning written in comments; no tool proves it, and no fuzzer has
  run over the table.
- The promise cannot hold as the spec states it. A call checks a
  reference against the length of its chunk, not against the bytes
  written so far, so that it never races with a participant appending
  to its own chunk. A damaged reference in a shared table can then
  point at bytes another process is writing: a data race, which no
  check can turn into an error.

## What Changes

- A new crate, `relhash`, holds the table: the index, the chunks and
  the records; the calls of a batch and the calls of one writer; the
  partitions by the bits of the hash; the Bloom filter of the keys; the
  marks of RIGHT and FULL joins; and the masks of rows, which move
  there from `tessera-core`. It depends on no crate of Tessera, works
  by batches only, and builds only for 64-bit targets. Its table that
  owns its memory, the way in without unsafe code and the table of its
  tests and benchmarks, sits behind a feature `alloc`, on by default.
- Tessera keeps what belongs to SQL or to its executor: the aggregate
  states and their folds, the items of a sort, the keys read from its
  columns, the phases of a shared build and the words of a spill. These
  use two calls the crate gives: cursors that read a record and change
  a payload by reference, and a merge of a chunk of records by the
  caller's rule.
- An error of a call is of one of two kinds, a wrong call or a damaged
  table, and names the rule it broke, instead of a text. A call
  allocates no memory.
- The promise over damaged bytes is narrowed to a table that no other
  process or thread writes while the call runs. Under it, the crate is
  checked by: a list of every place where the table reads its own
  memory; proofs by Kani of the unsafe code and of the checks; a matrix
  of damaged tables against every call; fuzzing on every pull request
  and for one to two hours before a release; mutants of the whole crate
  down to none that survive; Miri over all its tests.
- Every public item of the crate is documented; the documentation builds
  without warnings, its examples run as tests, and it links to the spec
  for every rule instead of repeating it. It recommends a build with
  LTO; Tessera is built and measured without it, so the crate must stay
  fast without it.
- `CONTRIBUTING.md` and `openspec/config.yaml` add a fifth surface that
  a spec may name, the public API of a crate of the workspace meant for
  use outside Tessera, one with `publish = true`; such an API changing
  needs a change; and a crate's documentation links to its spec.
- The requirements of `hash-table` that describe the table itself move
  to the capability `relhash` in two changes of their own,
  `relhash-spec-table` and `relhash-spec-partitions`, nine requirements
  each, once the code is in the crate. This change adds the new
  promises to that capability.
- The work goes as small pull requests into a branch `relhash`, one a
  task, each read by the maintainer; the branch goes into `main` whole
  at the end.
- The crate gets the first architecture contract of Tessera: rules that
  keep it free of Tessera and SQL, its memory behind one layer, its
  unsafe code where it is proven, its calls without allocation, its API
  small and its cost to Tessera nil, with a forecast of three likely
  changes; `check-specs.sh` learns to read contracts, from the base of
  a pull request.
- Nothing changes in the C API, in what SQL sees, or in a format. A
  step that only moves code leaves the machine code of every benchmark
  program as it was.

## Capabilities

### New Capabilities

### Modified Capabilities
- `relhash`: the crate of its own, its documentation, the masks of
  rows, the promise over any bytes, errors of two kinds, no memory
  taken, the same calls giving the same bytes, threads, records by
  reference, and a merge by the caller's rule. The capability is made
  by the changes `relhash-spec-table` and `relhash-spec-partitions`,
  which close first; this change adds its requirements to it, and its
  errors of two kinds take the place of the part of "Errors of a call"
  that moved.

## Impact

- Code: a new `crates/relhash`. `crates/tessera-kernels/src/table/`
  moves there, except `phases.rs`, `shared_spill.rs`, `order.rs` and the
  folds of `exclusive.rs`, which stay in the kernels; `row_mask.rs`,
  `bits.rs` and `bitmap.rs` of `tessera-core` move there, and
  `tessera-core` exports them again; `tessera-capi` takes the table
  from the new crate.
- C API: none; `include/tessera/table.h` does not change.
- SQL suites: none change.
- Tests: `crates/tessera-kernels/tests/table.rs` is split between the
  crate and the kernels; new: Kani proofs, the matrix of damage, fuzz
  targets, tests that count allocations, repeat a sequence and refuse
  to send a handle to another thread.
- CI: a Kani job; a short fuzz run on every pull request and a long one
  on request; Miri and mutants over the crate; the crate's tests alone,
  its documentation with warnings as errors, and a dry run of
  `cargo publish`; `make rust-loom` runs the models of the crate and of
  the kernels.
- Documents: the crate's `contract.md`, `CONTRIBUTING.md`,
  `openspec/config.yaml`, the crate's README and documentation, the
  design of `relhash` (its list of trust points, Files and Tests).
- Roadmap: no entry takes this up. On x86-64 the probe of a batch gives
  the processor no prefetch hint, since the hint is written for AArch64
  only; the entry `simd-primitives-avx2` already plans that hint, and
  once the table is in the crate it is written there.
- Process: a branch `relhash` takes a pull request for each task, with
  `main` merged into it as `main` moves; one pull request brings it into
  `main`.
