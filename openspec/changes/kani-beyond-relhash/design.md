# Design

## Context

Kani is a model checker for Rust. A harness, a function marked
`#[kani::proof]`, takes any value of its inputs (`kani::any()`), and
Kani proves that no input breaks an assertion, overflows or reads out of
bounds. The CI's Kani job (`.github/workflows/ci.yml`, about six
minutes, on pull requests) proves the checks of `relhash`, the float
casts and the bound of a date against a timestamp. It fails when a
`kani::cover!` in a harness is reached by no input: a proof around a
branch no input reaches would hold for no value at all. Kani compiles
only what a harness reaches.

Terms. An int4 is a 32-bit integer and an int8 a 64-bit one, as
PostgreSQL names them. A truncating division of `n` by `d` gives a
quotient, rounded toward zero, and a remainder, the rest: `n = q * d +
r`, with `r` of `n`'s sign and smaller than `d` in magnitude. A
selection word is 64 bits, one a row, of a batch's mask.

What Kani 0.68 (CBMC 6.11) did on this code, on an Apple M5 Pro, each
harness given 300 s; times are ratios more than absolutes, since other
runs shared the machine:

- the prepared divisor, checked against the `/` operator or against a
  remainder: int32 with any dividend and divisor, int32 with the fixed
  divisors 7 and 641, int64 with the divisor 7, and a 16-bit copy with
  any of both: all timed out (int32 by 7 with z3 too). One fixed
  divisor did verify, `i32::MAX`, whose multiplier has two bits set,
  in 0.1 s;
- the same divisors restricted to the powers of two, `±2^k` with `k`
  any shift, `i32::MIN` and `i64::MIN` among them, `Divisor::new` as it
  is: int32 verified in 2.1 s, int64 in 11.5 s;
- the walk of `WordValues`, copied without `anyhow`, every call of the
  reader checked inside it and the rows counted: by `next` in 120 s,
  by `fold` in 175 s. With the set of rows seen as its oracle and no
  check inside the reader, `next` took 43 s; the harnesses here keep
  both checks, and their time is measured when they are written;
- the sort item in six shapes, from one int4 to two int8 and an int4
  over four words: under two seconds each;
- `Decimal::round` with its division inside: scales 1 and 18 verified,
  scales 3 and 9 timed out;
- a harness that reaches `WordValues::try_new` as it is panics the Kani
  compiler: building an `anyhow::Error` reaches an intrinsic Kani 0.68
  does not handle;
- in a harness, `<[u64]>::cmp` left a check undetermined (pointers into
  one allocation), and a reachable `Option::unwrap` an unsupported
  construct (`caller_location`): the harnesses compare words by hand
  and match an `Option`.

The approach, for each target:

```
 production function            harness (#[kani::proof])
 ───────────────────            ────────────────────────
 try_new / try_fold_selected    any inputs
 / Encoder::encode                    │
       │                              │
       ├── the anyhow error            │
       │   (outside the proof)        │
       ▼                              ▼
   private helper  ◄──────────── the same helper,
   (no anyhow)                   assertions on what it gives
```

## Goals / Non-Goals

**Goals:**

- Prove for every input the promises of the two walks over a selection
  word that unsafe readers of `tessera-capi` rely on.
- Prove that a sort item orders as its row does, for shapes that reach
  every branch of the packing of bits into words.
- Prove the prepared divisors of int4 and int8 where Kani reaches:
  every power of two.
- Keep the machine code of every function touched as it is.

**Non-Goals:**

- Proofs of a division by a value that is not a power of two.
- Proofs of the vector forms (NEON, AVX2): Kani does not model their
  intrinsics; the differential tests on both architectures hold them to
  the scalar code.

## Decisions

### 1. `WordValues` is checked and built by one helper

- **Was:** `try_new` checks the word's index, the padding bits and the
  readiness with three `ensure!`s, each of which builds an
  `anyhow::Error` on failure, then builds the iterator.
- **Will be:** one private helper takes `nrows`, the word's index, the
  selected and the prepared bits and the reader; it checks and builds
  the iterator, or returns the reason of a refusal, a `&'static str`.
  It never calls the reader. `try_new` calls it and turns a reason into
  the same `anyhow` error, with the same text; nothing else.
- **Proofs** (`tessera-core/src/reader/proofs.rs`): with any `nrows`,
  index, selected and prepared bits, through the helper:
  - a refusal comes with no call of the reader;
  - the reader, checked inside each call, is called only for a row of
    the word that is selected, prepared and below `nrows`, each after
    the last, and the row it is called for is the row given;
  - the walk by `next` and the walk by `fold` each give exactly the
    selected rows, in increasing order: the set of rows seen, one bit a
    row, equals the selection, which a row given twice and one skipped
    would not.
  Covers: a full word (the counted loop of `fold`), a sparse one, an
  empty one, and each of the three refusals.
- **Refused:** a checker without a reader and a constructor apart from
  it: the proofs could not see a wrapper that reads before it checks.
  Stubbing `anyhow`'s constructor with `kani::stub` needs an unstable
  flag and pins a dependency's private functions.

### 2. The second walk, `try_fold_words` of `tessera-capi`

- **Was:** the readers of `tessera-capi` fold a whole selection through
  `try_fold_words`, a walk of their own that checks the row counts and
  the readiness with `ensure!` and reads with `get_unchecked` under it.
- **Will be:** the same as decision 1: its checks and its walk in a
  helper without `anyhow`, the error built outside it.
- **Proofs** (`tessera-capi/src/column/proofs.rs`): with any selection,
  readiness and NULL bits of up to two words (128 rows):
  - the reader is called only for selected, prepared rows, each once,
    in increasing order, and the fold step gets the row read;
  - a row count that differs from the selection's reads nothing;
  - a word with an unprepared selected row is refused before any of
    its rows is read, and nothing after it is read; the rows of the
    words before it may have been read already, as today;
  - a fold step that fails stops the walk: no row after it is read;
  - with no refusal and no failure, every selected row is read.
  Covers: a full word, a sparse one after it, a refusal of the second
  word after reads of the first, and a failing fold step. The bound of
  two words reaches each of these; the loop over words repeats the same
  body.

### 3. The sort key's encoding without `anyhow` in its path

- **Was:** `Encoder::encode` writes an item and bails out with an
  `anyhow` error on a NULL in a key declared not nullable.
- **Will be:** the bits are written by a private helper that returns
  the item or the index of the key that refused its NULL; `encode`
  turns the index into the same error. Both stay `#[inline(always)]`.
- **Proofs** (`tessera-kernels/src/sort/proofs.rs`): for any two rows
  and any directions and NULL places, their items compare word by word
  as the rows compare by their keys, NULL placed by its key and a
  descending key reversed, then by reference; their key bits alone
  (`key_bits`, what a run of an external sort keeps) compare by the
  keys only; a NULL in a key declared not nullable is refused. Each
  harness asserts that its keys' bits and the reference fit its words.
  The shapes:
  - one int4, not nullable: 64 bits, one word;
  - one int8, not nullable: the key fills the first word exactly;
  - two int4, not nullable: the second key ends on the word's last
    bit;
  - an int8 and an int4, nullable: 130 bits, three words;
  - two int8 and an int4, nullable: 195 bits, four words, keys across
    both boundaries;
  - three int4, nullable: 131 bits, three words.
  They reach each branch of the packing (a key inside a word, ending on
  its last bit, split across two); they do not prove every layout of up
  to 16 keys. An int4 key's slot holds the value widened with its sign,
  as the records keep it; the proof assumes so.

### 4. The prepared divisors of the powers of two

- **Was:** the property tests check the divisors of every shape at 32
  and 64 bits; the int4 divisor's `quotient` and `remainder` exist for
  the tests only.
- **Will be:** the int4 divisor's `quotient` and `remainder` are
  compiled for Kani too.
- **Proofs** (`tessera-kernels/src/int32/divisor/proofs.rs` and
  `int64/divisor/proofs.rs`): for every power of two, either sign, and
  every dividend, `Divisor::new` as it is gives a divisor whose
  quotient and remainder equal the core's truncating ones, the
  quotient checked against the shift, with no division in the
  harness. This is the path of the bias, the shift and the sign, with
  no multiplier; the int8 one is the loop that divides a word's lanes
  on AArch64.

### 5. The divisors by a multiplier: the maintainer chooses

Kani does not finish `quotient(n) == n / d` for a divisor by a
multiplier in general, at any width tried, 16 bits included, nor for
the fixed divisors 7 and 641; a single one, `i32::MAX`, verified. The
change leaves these divisors out by choice of scope. Two ways are
left:

- **Keep the property tests** (the default): the divisors of every
  shape and the dividends where a wrong multiplier shows, both signs, at
  32 and 64 bits, as today. No code changes.
- **One source for every width, checked exhaustively when small:** a
  macro writes the divisor for 8, 16, 32 and 64 bits from one text. A
  test checks the 8-bit one for every dividend and divisor (65 536
  pairs, in every test run) and the 16-bit one for all 4.3 billion
  pairs (about ten seconds in a release build, in the release test
  run). A slip of the multiplier's rounding is the same at every width,
  so it shows at 8 or 16 bits; a slip of a width's constants does not.
  The 32- and 64-bit divisors sit in hot loops, so their machine code
  must stay the same (`tessera-bench --disasm`).

### 6. `Decimal::round` and `split` stay with their tests

`round` divides by the power of ten of the decimal's scale, a
constant for the scales 1 to 4 and picked at run time for 5 to 18;
`split` divides by the microseconds of a day. Both then correct the
quotient by the rest in two or three lines. The division is beyond
Kani where it matters (`round` at scales 3 and 9), and a proof of the
correction alone would restate it. The tests keep both, as today.

## What we do not do

- No proof of the vector code: Kani does not model NEON or AVX2.
- No newer Kani: 0.68.0 is the latest release.
- No proof of a division by a value that is not a power of two.

## Risks / Trade-offs

- [The Kani job grows by the walks, a few minutes on the machine
  measured, likely more on CI's runners] → it runs on pull requests
  only. Every Kani step of `ci.yml` gets `--harness-timeout 600`, so a
  harness that slows down fails in ten minutes, not at the job's 60.
- [A harness bounds what it proves: two words of `try_fold_words`, the
  sort shapes listed] → each bound is written in the harness's
  documentation and here.
- [What the proofs do not reach] → a caller that passes another word's
  bits, or values shorter than its rows; the extraction of a batch's
  keys into slots and NULL bits before `encode`; the sort's 128-bit
  key for items of two words; `run_words`; a walk of `WordValues`
  that mixes `next` and `fold`; the division of `round` and `split`.
  These stay with their tests.
- [A new Kani or Rust may break the compilation of a harness, as
  `anyhow` does today] → Kani is pinned in CI; a failure there shows on
  the pull request that updates it.
- [Moving checks into helpers may change the machine code] →
  `tessera-bench --disasm` compares it with the base, and a difference
  in a loop is explained or undone.

## Measurement plan

No timing: the change moves no work. `cargo run --release --locked -p
tessera-bench -- --base <base> --disasm` over the programs that read
columns, aggregate and sort, and `--disasm --module kernels`: the loops
of the two walks, the sort items and the divisors the same as at the
base.

## Decisions of the maintainer

- Pending: decision 5, the divisors by a multiplier.

## Files

- `crates/tessera-core/src/reader.rs`, `reader/proofs.rs`
- `crates/tessera-capi/src/column.rs`, `column/proofs.rs`
- `crates/tessera-kernels/src/sort.rs`, `sort/proofs.rs`
- `crates/tessera-kernels/src/int32/divisor.rs`, `divisor/proofs.rs`
- `crates/tessera-kernels/src/int64/divisor.rs`, `divisor/proofs.rs`
- `.github/workflows/ci.yml`, `.cargo/mutants.toml`

## Tests

- The harnesses of decisions 1 to 4, run by the Kani job of CI.
- Each harness fails when the code it proves is broken on purpose: a
  padding check dropped, the reader called before the checks, a row
  given twice, a NULL bit inverted, `<=` turned into `<` where a key
  meets the room left in a word, a power of two's bias off by one.
- The existing tests of the modules, unchanged.
