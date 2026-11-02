# Proposal

## Why

Two pieces of Rust that unsafe code and the order of results rely on
are judged only by tests that sample their inputs. `WordValues` of
`tessera-core` promises the rows of one selection word: in range,
selected, prepared, once each and in order. `tessera-capi` reads values
with `get_unchecked` on that promise alone
(`crates/tessera-capi/src/column/ints.rs`). The encoding of a sort key
turns a row's keys into bits whose unsigned order is the rows' order:
NULL first or last, a descending key inverted, keys packed across
64-bit words. A slip there sorts rows wrongly with no error. Kani, which
the CI already runs for `relhash`, the float casts and the date bound,
proves such bit-level code for every input.

The roadmap's entry `kani-beyond-relhash` also names the prepared
divisors of int32 and int64, `Decimal::round` and the split of a
timestamp. A spike on Kani 0.68 (CBMC 6.11) measured what is within
reach, each harness given 300 s:

- the prepared divisor, `quotient(n) == n / d`: int32 with any `n` and
  `d`, int32 with any `n` and four fixed `d`, int64 with any `n` and
  `d = 7`, and even a 16-bit copy with any `n` and `d`: all four timed
  out. A 64-bit division by a constant timed out before too, in the
  split of a timestamp;
- `WordValues`, a copy without `anyhow`: the walk of `next` verified in
  117 s and that of `fold` in 148 s. The crate itself does not compile
  under Kani 0.68: building an `anyhow` error reaches an intrinsic that
  panics the Kani compiler;
- the encoding of a sort key, two nullable keys of any direction and
  NULL place, an int8 and an int4: verified in 2 s.

## What Changes

- `WordValues::try_new` takes its checks from a private function that
  returns the reason of a refusal, so that the proofs reach the checks
  and the walk without `anyhow`. Proofs in `tessera-core`: a refusal
  reads nothing; `next` and `fold` give each selected row of the word
  once, in increasing order, below `nrows` and prepared, at most 64 of
  them. `cargo kani -p tessera-core` joins the Kani job of CI.
- Proofs of the sort key in `tessera-kernels`: for every pair of rows,
  their items compare as the rows do by their keys, NULL placed and a
  descending key reversed, and then by reference; with keys that cross
  the boundary of a word.
- `Decimal::round`: the rule after the division, rounding half away
  from zero, proven over the quotient and the rest for each scale from
  1 to 18, with no overflow. The division by the scale's power of ten
  is the compiler's and stays with the tests.
- The split of a timestamp is left out: the date bound's proof already
  takes a timestamp as its day and time, and the split's own fixup
  cannot be stated without a multiplication.
- The prepared divisors are left out of Kani. Unless the maintainer
  chooses otherwise, they keep their property tests. The other way
  weighed is in design.md: one source of the divisor for every width,
  checked exhaustively at 8 and 16 bits.
- No behavior, C API, format or setting changes.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

None. The change adds proofs and moves checks inside two functions;
no promise changes, so the change declares `skip_specs`.

## Impact

- Code: `crates/tessera-core/src/reader.rs` (the checks of
  `WordValues::try_new`, a `proofs` module under `cfg(kani)`);
  `crates/tessera-kernels/src/sort.rs` and `decimal.rs` (`proofs`
  modules; the rounding rule out of its closure).
- CI: the Kani job of `.github/workflows/ci.yml` runs
  `tessera-core` too; `.cargo/mutants.toml` excludes the new proof
  files, which mutant builds never compile.
- Capabilities, public headers, settings, SQL suites: none.
- The machine code of `WordValues` and `Decimal::round` must stay the
  same (`tessera-bench --disasm`).
- Takes up the roadmap's entry `kani-beyond-relhash`.
