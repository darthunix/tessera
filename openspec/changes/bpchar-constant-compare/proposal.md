## Why

The text kernel's equality and inequality of a char(n) column with a
scalar counted the trailing spaces of both strings on every row before
comparing them: the padding of a char(10) walked for every row, and the
scalar's too. TPC-H Q19 compares `l_shipmode` and `l_shipinstruct` with
constants, and the kernel was 29 % to 32 % of its profile; Q12 and Q10
compare char(n) columns with constants as well. Greengage runs the same
kernels.

## What Changes

- `Strings`, the source of a text kernel's strings, gets `constant()`:
  the one string of a scalar, `None` for a column. The C entry point's
  scalar source answers it.
- `compare` trims a bpchar scalar once a call and compares each value's
  first bytes with it, looking at the rest for spaces only when they
  match. The result is `bpchareq`'s and `bpcharne`'s under a
  deterministic collation, the only one the kernel is called under.
- Tests: the trimmed comparison against the old one for every pair of
  nine short strings; a property test of random bpchar columns against
  a scalar read row by row; `tess_text_compare` with a bpchar scalar on
  either side.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

None: the kernel's contract and its results stay. The change declares
`skip_specs: true`.

## Impact

- Code: `crates/tessera-kernels/src/text.rs`,
  `crates/tessera-capi/src/c/text.rs`.
- Tests: `crates/tessera-kernels/tests/text.rs`,
  `crates/tessera-capi/tests/text.rs`; `.cargo/mutants.toml` excludes
  the scalar's `constant()` replaced by `None`, which gives the same
  answer row by row.
- Documents: `docs/kernels.md`.
- Runs: TPC-H Q19, Q12, Q10, with Q1 and Q6 as controls.
