## Why

A batch function is a function of PostgreSQL that Tessera computes over
a batch of up to 64 rows at a time, instead of row by row through the
core. Most of Tessera's batch functions are written in Rust, in the
crate `tessera-kernels`, and called from C through the crate
`tessera-capi`. The functions of integers, dates, timestamps, numeric
and text work this way. The functions of `float4` and `float8` are the
last ones still written in C, in `kernels/float.c` (385 lines). They
are 58 functions: comparisons, arithmetic, negation, `abs` and casts.

This matters for three reasons.

- **No Rust check reaches them.** Only the SQL suite `types` tests
  them. No Rust test, property test, mutant or proof can, because
  they are not in Rust. Every other family of kernels has Rust tests
  that compare it with a reference, and a mutant run that judges
  those tests.
- **Their rules are the core's, copied by hand.** NaN is equal to
  itself and greater than every number; -0 equals 0; a cast to an
  integer rounds half to even and checks its range; a float4 result
  that becomes infinite or zero from finite, non-zero operands is an
  error. Today C gets these rules by calling the inline functions of
  `utils/float.h`, row by row. A Rust version must write them down in
  one place, where a test can compare them with the core's answers.
- **They raise errors inside the row loop.** The C loop calls
  `ereport` when a row overflows, which jumps out of the kernel. Every
  Rust kernel instead returns a status, and C raises it after the
  call. The float functions are the last exception.

This change takes up the roadmap entry `float-kernels-in-rust`.

## What Changes

- `tessera-kernels` gets a module `float` (safe Rust): the comparisons
  in the core's order, the arithmetic of float4 and float8 with the
  core's checks of overflow, underflow and division by zero, and the
  casts between floats and integers, each as a batch function over the
  selected rows, as the calendar's are.
- `tessera-capi` gets the entry points of the float functions, declared
  in a new public header `include/tessera/float.h`.
- `kernels/float.c` keeps only what needs PostgreSQL: the table of the
  58 functions with their OIDs and their registration. Each call goes
  once a batch into Rust and raises the status Rust returns.
- An error is reported at the same row, with the same SQLSTATE and the
  same message as today. The results of every query stay the same.
- No **BREAKING** change: SQL behavior and settings do not change; the
  C API only grows.

## Capabilities

### New Capabilities

None. The kernels have no capability folder yet; their functions are
described in `docs/function.md`, which this change updates.

### Modified Capabilities

None. No requirement of an existing spec changes: what a query returns
stays the same. The change sets `skip_specs: true`.

## Impact

- Public headers: a new `include/tessera/float.h`.
- Code: `kernels/float.c` (shrinks to the registration);
  `crates/tessera-kernels/src/float.rs` (new);
  `crates/tessera-kernels/src/error.rs` (the new error is classified);
  `crates/tessera-capi/src/c/float.rs` (new);
  `crates/tessera-capi/src/c/source.rs` (takes the reading of a column
  or a scalar Datum that `c/calendar.rs` has today, so that both
  families share it).
- Tests: Rust tests of the module and of the entry points (Miri runs
  the latter); the SQL suite `types` must give the same output byte
  for byte, with a few edge cases added.
- Benchmarks: three cases of floats in the family `win` of
  `bench/pg`, for the A/B against the C version.
- Settings, formats, plans and EXPLAIN: unchanged.
- Documents: `docs/function.md`, the paragraph on the float functions.
