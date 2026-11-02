# Design

## Context

Kani is a model checker for Rust: a harness, a function marked
`#[kani::proof]`, takes any value of its inputs (`kani::any()`), and
Kani proves that no input breaks an assertion, overflows or reads out of
bounds. The CI's Kani job (`.github/workflows/ci.yml`, about six
minutes, on pull requests) proves the checks of `relhash`, the float
casts and the bound of a date against a timestamp. It fails when a
`kani::cover!` in a harness is reached by no input: a proof around an
unreachable branch would hold for no value at all.

What Kani 0.68 does and does not do, measured for this change (see
proposal.md, Why):

- bit operations, comparisons and short loops over a 64-bit word
  verify in seconds to minutes;
- a symbolic multiplication or division on 16 bits or more does not
  finish in 300 s;
- building an `anyhow::Error` reaches an intrinsic that panics the Kani
  compiler, so a harness must not reach a function that can return one;
- `<[T]>::cmp` (pointers into one allocation) and `Option::unwrap`
  (`caller_location`) are not supported in a harness.

## Goals / Non-Goals

**Goals:**

- Prove for every input the promise of `WordValues` that unsafe readers
  of `tessera-capi` rely on.
- Prove that a sort item orders as its row does, for the shapes of keys
  that cross the boundary of a 64-bit word.
- Prove the rounding rule of `Decimal::round` after its division.
- Keep the machine code of the three functions as it is.

**Non-Goals:**

- Proofs of a division or a multiplication by a symbolic value: out of
  Kani's reach today.
- Proofs of the vector forms (NEON, AVX2): Kani does not model their
  intrinsics; the differential tests on both architectures hold them to
  the scalar code.

## Decisions

### 1. The checks of `WordValues::try_new` go to a function of their own

- **Was:** `try_new` checks the word's index, the padding bits and the
  readiness with three `ensure!`s, each of which builds an
  `anyhow::Error` on failure.
- **Will be:** a private function takes `nrows`, the word's index, the
  selected and the prepared bits and returns the reason of a refusal,
  a `&'static str`, or none. `try_new` calls it and turns a reason into
  the same error, with the same text. A private constructor builds the
  iterator, so that the harness and `try_new` take one path.
- **Proofs** (`tessera-core/src/reader/proofs.rs`): with any `nrows`,
  index, selected and prepared bits, and the function giving no
  reason, the walk by `next` and the walk by `fold` each give every
  selected row of the word exactly once, in increasing order, each
  below `nrows` and prepared, and the reader is called once per row and
  for no other. A refusal is reached before the iterator exists, so no
  read can come before it. Covers: a full word (the counted loop of
  `fold`), a sparse one, an empty one, each refusal.
- **Refused:** stubbing `anyhow`'s constructor with `kani::stub`, which
  needs an unstable flag and pins a dependency's private functions;
  proving a copy of the code, which proves nothing about the crate.

### 2. The sort key's encoding without `anyhow` in its path

- **Was:** `Encoder::encode` writes an item and bails out with an
  `anyhow` error on a NULL in a key declared not nullable.
- **Will be:** the bits are written by a private function that returns
  the item or the index of the key that refused its NULL; `encode`
  turns the index into the same error. Both stay `#[inline(always)]`.
- **Proofs** (`tessera-kernels/src/sort/proofs.rs`): for any two rows,
  any directions and NULL places, their items compare word by word as
  the rows compare by their keys, NULL placed by its key and a
  descending key reversed, then by reference; their key bits alone
  (`key_bits`, what a run of an external sort keeps) compare by the
  keys only; a NULL in a key declared not nullable is refused. Shapes:
  one int4 not nullable (one word); an int8 and an int4, nullable (130
  bits, three words); two int8 and an int4, nullable (195 bits, four
  words, keys across both boundaries); three nullable int4 (131 bits).
  The words are compared by hand, not with `<[u64]>::cmp`.
- **Refused:** every shape up to the 16 keys a sort takes: the shapes
  above already put a key across each position of a boundary that the
  packing code tells apart (inside a word, ending on its last bit,
  split across two).

### 3. The rounding rule of `Decimal::round` out of its closure

- **Was:** a closure inside `round` divides by the scale's power of ten
  and rounds the quotient by the rest.
- **Will be:** a private `#[inline(always)]` function takes the
  quotient, the rest and the unit and rounds; `round` divides and calls
  it.
- **Proof** (`tessera-kernels/src/decimal/proofs.rs`): for every scale
  1 to 18 and every quotient and rest a decimal's value can give (the
  rest smaller than the unit in magnitude, of the value's sign, the
  quotient within the value's limit over the unit), the result is the
  quotient moved one away from zero when twice the rest's magnitude
  reaches the unit, else the quotient, and nothing overflows.
- The division itself is the compiler's, by a constant; it stays with
  the tests, as Kani cannot finish it.

### 4. The split of a timestamp stays with its tests

`split` divides by the microseconds of a day and corrects a negative
rest. Its correction cannot be stated without multiplying the day back,
which Kani does not finish; the proof of the date bound already takes
a timestamp as its day and time. No proof is added.

### 5. The prepared divisors: the maintainer chooses

Kani cannot prove `quotient(n) == n / d` at any width tried, 16 bits
included. Two ways are left:

- **Keep the property tests** (the default): the divisors of every
  shape and the dividends where a wrong multiplier shows, both signs, at
  32 and 64 bits, as today. No code changes.
- **One source for every width, checked exhaustively when small:** a
  macro writes the divisor for 8, 16, 32 and 64 bits from one text. A
  test checks the 8-bit one for every dividend and divisor (65 536
  pairs, in every test run) and the 16-bit one for all 4.3 billion
  pairs (about ten seconds in a release build, in the release test
  run). A slip of the bias, the shift or the sign is the same at every
  width, so it shows at 8 or 16 bits; a slip of a width's constants
  does not. The 32- and 64-bit divisors sit in hot loops, so their
  machine code must stay the same (`tessera-bench --disasm`).

## What we do not do

- No proof of the vector code: Kani does not model NEON or AVX2.
- No newer Kani: 0.68.0 is the latest release.
- No proof of a division or multiplication by a symbolic value.

## Risks / Trade-offs

- [The Kani job grows by about five minutes, the two walks of
  `WordValues` at about two minutes each] → it runs on pull requests
  only, with a limit of 60 minutes.
- [A harness bounds what it proves: three keys at most, the shapes
  listed] → each bound is written in the harness and here.
- [A new Kani or Rust may break the compilation of a harness, as
  `anyhow` does today] → Kani is pinned in CI; a failure there is seen
  on the pull request that updates it.
- [Moving checks out of a function may change its machine code] →
  `tessera-bench --disasm` compares it with the base, and a difference
  in a loop is explained or undone.

## Measurement plan

No timing: the change moves no work. `cargo run --release --locked -p
tessera-bench -- --base <base> --disasm` over the programs that read
columns and sort, and `--disasm --module kernels`: the loops of
`WordValues`, the sort items and `Decimal::round` the same as at the
base.

## Decisions of the maintainer

- Pending: decision 5, the prepared divisors.

## Files

- `crates/tessera-core/src/reader.rs`, `reader/proofs.rs`
- `crates/tessera-kernels/src/sort.rs`, `sort/proofs.rs`
- `crates/tessera-kernels/src/decimal.rs`, `decimal/proofs.rs`
- `.github/workflows/ci.yml`, `.cargo/mutants.toml`

## Tests

- The harnesses of decisions 1 to 3, run by the Kani job of CI.
- Each harness fails when the code it proves is broken on purpose: a
  padding check dropped, a NULL bit inverted, a rounding branch turned.
- The existing tests of the three modules, unchanged.
