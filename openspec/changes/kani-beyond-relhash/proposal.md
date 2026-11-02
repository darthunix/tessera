# Proposal

## Why

Unsafe code of `tessera-capi` reads values with `get_unchecked`
(`crates/tessera-capi/src/column/ints.rs`), trusting one of two walks
over a selection word to give only rows that are in range, selected and
prepared, once each and in order: `WordValues` of `tessera-core`, and
`try_fold_words` of `tessera-capi`. The encoding of a sort key turns a
row's keys into bits whose unsigned order is the rows' order: NULL
first or last, a descending key inverted, keys packed across 64-bit
words; a slip sorts rows wrongly with no error. The prepared divisors
replace a division by a multiplication, a bias and shifts in the
kernels' loops. All of these are judged today by tests that sample
their inputs. Kani, which the CI already runs for `relhash`, the float
casts and the date bound, proves such code for every input.

The roadmap's entry `kani-beyond-relhash` named the divisors,
`Decimal::round` and the split of a timestamp as well. Spikes on Kani
0.68 (design.md, Context) showed what is within reach:

- the walks of `WordValues`, every call of the reader checked: about
  one to three minutes each. A harness that reaches `try_new` as it is
  panics the Kani compiler, on the path that builds an `anyhow` error;
- the sort item, in six shapes up to four words: under two seconds
  each;
- the divisors by a power of two, any sign and shift: 2.1 s (int4) and
  11.5 s (int8). A divisor by a multiplier did not finish in 300 s,
  in general at 16, 32 and 64 bits nor for the fixed divisors 7 and
  641, though `i32::MAX` alone did; nor did `Decimal::round` at scales
  3 and 9.

## What Changes

- `WordValues::try_new` and `try_fold_words` are each checked and
  walked by a private helper that never builds an `anyhow` error and
  never calls the reader before its checks; the public functions only
  turn the helper's reason into the same error. Proofs: the reader is
  called only for selected, prepared rows in range, once each, in
  increasing order; a refused word is not read, nor anything after a
  refusal or a failed fold step; with neither, every selected row is
  read. `try_fold_words` is proven over up to two words. `cargo kani
  -p tessera-core` and `-p tessera-capi` join the Kani job of CI.
- Proofs of the sort key in `tessera-kernels`: for every pair of rows,
  their items compare as the rows do by their keys, NULL placed and a
  descending key reversed, and then by reference; in six shapes that
  reach each branch of the packing of bits into words. They do not
  prove every layout of up to 16 keys.
- Proofs of the prepared divisors of int4 and int8 for every power of
  two and every dividend: quotient and remainder equal the truncating
  ones.
- The divisors by a multiplier are left out of Kani. Unless the
  maintainer chooses otherwise, they keep their property tests; the
  other way, one source of the divisor checked exhaustively at 8 and 16
  bits, is in design.md.
- `Decimal::round` and `split` stay with their tests: their divisions
  are beyond Kani where it matters, and a proof of the lines after
  them would restate them.
- Every Kani step of CI gets a limit of ten minutes a harness.
- No behavior, C API, format or setting changes.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

None. The change adds proofs and moves code of three functions into
helpers; no promise changes, so the change declares `skip_specs`.

## Impact

- Code: `crates/tessera-core/src/reader.rs`,
  `crates/tessera-capi/src/column.rs`,
  `crates/tessera-kernels/src/sort.rs` (helpers and `proofs` modules
  under `cfg(kani)`); `int32/divisor.rs` and `int64/divisor.rs`
  (`proofs` modules; the int4 `quotient` compiled for Kani too).
- CI: the Kani job of `.github/workflows/ci.yml` runs `tessera-core`
  and `tessera-capi` too, each step with `--harness-timeout 600`;
  `.cargo/mutants.toml` excludes the new proof files, which mutant
  builds never compile.
- Capabilities, public headers, settings, SQL suites: none.
- The machine code of the two walks, the sort items and the divisors
  must stay the same (`tessera-bench --disasm`).
- Takes up the roadmap's entry `kani-beyond-relhash`.
