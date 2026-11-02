# Tasks

## 0. How the work goes

Two pull requests: sections 2 and 3, then section 4 and the close; the
divisors (section 5) only if the maintainer chooses them. A task is one
commit that builds and passes the checks by itself. Each commit that
changes Rust is checked by `make rust-check` and by `tessera-bench
--base <previous> --disasm` on the programs it touches and with
`--module kernels`; a loop that changes is explained or the change
undone. Each harness is shown to fail when the code it proves is
broken on purpose, the broken file restored and touched after. Kani
runs as `cargo kani --harness-timeout 600 -Z unstable-options -p
<crate>`.

## 1. The proposal

- [x] 1.1 Write the proposal, the design and the tasks of this change,
      with `skip_specs`. Check: `openspec validate kani-beyond-relhash
      --strict`; `.github/scripts/check-specs.sh`.

## 2. WordValues

- [ ] 2.1 Move the checks of `WordValues::try_new` into a private
      function that returns the reason of a refusal, and the building of
      the iterator into a private constructor; `try_new` keeps its
      errors and their texts. Check: `cargo test --locked -p
      tessera-core -p tessera-capi`; `tessera-bench --disasm` on
      `column_reader` and the aggregate programs, the same loops.
- [ ] 2.2 The proofs of `WordValues` in `tessera-core/src/reader/
      proofs.rs` (design, decision 1), with their covers; `cargo kani
      -p tessera-core` joins the Kani job of `ci.yml` with its cover
      check, and the job's comment names the new proofs;
      `.cargo/mutants.toml` excludes the file. Check: `cargo kani -p
      tessera-core`, every cover satisfied; each harness fails with the
      padding check dropped and with a row read twice.

## 3. The sort key

- [ ] 3.1 Write the bits of a sort item in a private function that
      returns the item or the key that refused its NULL; `Encoder::
      encode` keeps its error and text. Check: `cargo test --locked -p
      tessera-kernels sort`; `tessera-bench --disasm` on the sorting
      programs and `--module kernels`, the same loops.
- [ ] 3.2 The proofs of the sort key in `tessera-kernels/src/sort/
      proofs.rs` (design, decision 2): the four shapes, both directions
      and NULL places, items and key bits, and the refusal of a NULL;
      `.cargo/mutants.toml` excludes the file. Check: `cargo kani -p
      tessera-kernels`, every cover satisfied; the harnesses fail with
      a NULL bit inverted and with a descending int4 inverted on all 64
      bits.

## 4. The rounding of a decimal

- [ ] 4.1 The rounding rule of `Decimal::round` out of its closure into
      a private `#[inline(always)]` function of the quotient, the rest
      and the unit. Check: `cargo test --locked -p tessera-kernels
      decimal`; `tessera-bench --disasm --module kernels`, the same
      code.
- [ ] 4.2 Its proof in `tessera-kernels/src/decimal/proofs.rs` (design,
      decision 3), the scale any of 1 to 18; `.cargo/mutants.toml`
      excludes the file. Check: `cargo kani -p tessera-kernels`, every
      cover satisfied; the harness fails with `>=` turned into `>`.

## 5. The prepared divisors, if the maintainer chooses them

- [ ] 5.1 The way of design decision 5 the maintainer chose: none, or
      the divisor written once for every width by a macro, with the
      exhaustive tests at 8 and 16 bits. Check: `cargo test --locked -p
      tessera-kernels divisor` in the debug and release runs;
      `tessera-bench --disasm` on `arith_int32`, `arith_int64` and
      `--module kernels`, the same loops.

## 6. The checks of the whole and the close

- [ ] 6.1 The mutants of the changed lines: `cargo mutants --in-diff`
      of the diff against the base, none left alive or each listed with
      its reason.
- [ ] 6.2 Outside reviews by Codex and Fable 5.1 of each pull request,
      their findings fixed or placed.
- [ ] 6.3 Close the change: its folder goes, the entry leaves the
      roadmap, the bounds of the proofs and the left-out targets (the
      split, the divisors if not chosen) go into the designs of the
      code or the roadmap; the pull request says what was proven, the
      Kani times, and what was left. Check:
      `.github/scripts/check-specs.sh`.
