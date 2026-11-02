# Tasks

## 0. How the work goes

Two pull requests: sections 2 and 3, then sections 4 to 6 with the
close; section 6 only if the maintainer chooses it. A task is one
commit that builds and passes the checks by itself. Each commit that
changes Rust is checked by `make rust-check` and by `tessera-bench
--base <previous> --disasm` on the programs it touches and with
`--module kernels`; a loop that changes is explained or the change
undone. Each harness is shown to fail when the code it proves is
broken on purpose, the broken file restored and touched after. Kani
runs locally and in CI as `cargo kani --harness-timeout 600 -Z
unstable-options -p <crate>`.

Before each pull request, not as commits of their own: the mutants of
its changed lines (`cargo mutants --in-diff` of its diff), none left
alive or each listed in `.cargo/mutants.toml` with its reason; and the
outside reviews by Codex and Fable 5.1, their findings fixed or
placed.

## 1. The proposal

- [x] 1.1 Write the proposal, the design and the tasks of this change,
      with `skip_specs`. Check: `openspec validate kani-beyond-relhash
      --strict`; `.github/scripts/check-specs.sh`.

## 2. WordValues

- [ ] 2.1 One private helper that checks the word and builds the
      iterator, or gives the reason of a refusal, never calling the
      reader; `try_new` only turns a reason into its error, with the
      same text. Check: `cargo test --locked -p tessera-core -p
      tessera-capi`; `tessera-bench --disasm` on `column_reader` and
      the aggregate programs, the same loops.
- [ ] 2.2 The proofs of `WordValues` in `tessera-core/src/reader/
      proofs.rs` (design, decision 1), with their covers; `cargo kani
      -p tessera-core` joins the Kani job of `ci.yml` with its cover
      check, every Kani step there gets `--harness-timeout 600 -Z
      unstable-options`, and the job's comment names the new proofs;
      `.cargo/mutants.toml` excludes the file. Check: `cargo kani -p
      tessera-core`, every cover satisfied; the harnesses fail with the
      padding check dropped, with the reader called before the checks,
      and with a row given twice.

## 3. The walk of tessera-capi

- [ ] 3.1 The checks and the walk of `try_fold_words` in a helper that
      builds no `anyhow` error; the error is built outside it, with the
      same text. Check: `cargo test --locked -p tessera-capi`;
      `tessera-bench --disasm` on `column_reader` and the aggregate
      programs, the same loops.
- [ ] 3.2 Its proofs in `tessera-capi/src/column/proofs.rs` (design,
      decision 2), up to two words; `cargo kani -p tessera-capi` joins
      the Kani job; `.cargo/mutants.toml` excludes the file. Check:
      `cargo kani -p tessera-capi`, every cover satisfied; the
      harnesses fail with the readiness check dropped and with a full
      word read from its second row.

## 4. The sort key

- [ ] 4.1 Write the bits of a sort item in a private helper that
      returns the item or the key that refused its NULL; `Encoder::
      encode` keeps its error and text. Check: `cargo test --locked -p
      tessera-kernels sort`; `tessera-bench --disasm` on the sorting
      programs and `--module kernels`, the same loops.
- [ ] 4.2 The proofs of the sort key in `tessera-kernels/src/sort/
      proofs.rs` (design, decision 3): the six shapes, both directions
      and NULL places, items and key bits, and the refusal of a NULL;
      `.cargo/mutants.toml` excludes the file. Check: `cargo kani -p
      tessera-kernels`, every cover satisfied; the harnesses fail with
      a NULL bit inverted, with a descending int4 inverted on all 64
      bits, and with `<=` turned into `<` where a key meets the room
      left in a word.

## 5. The divisors of the powers of two

- [ ] 5.1 The int4 divisor's `quotient` and `remainder` compiled for
      Kani as for the tests, and the proofs of both divisors in
      `int32/divisor/proofs.rs` and `int64/divisor/proofs.rs` (design,
      decision 4); `.cargo/mutants.toml` excludes the files. Check:
      `cargo kani -p tessera-kernels`, every cover satisfied; the
      harnesses fail with the bias of a power of two off by one;
      `tessera-bench --disasm` on `arith_int64`, the same loops.

## 6. The divisors by a multiplier, if the maintainer chooses

- [ ] 6.1 The way of design decision 5 the maintainer chose: none, or
      the divisor written once for every width by a macro, with the
      exhaustive tests at 8 and 16 bits. Check: `cargo test --locked -p
      tessera-kernels divisor` in the debug and release runs;
      `tessera-bench --disasm` on `arith_int32`, `arith_int64` and
      `--module kernels`, the same loops.

## 7. The close

- [ ] 7.1 Close the change: its folder goes and its entry leaves the
      roadmap. The bounds of each proof stay in its module's
      documentation and in the comment of the Kani job of `ci.yml`; the
      divisors by a multiplier, `Decimal::round` and `split`, left out,
      go into `openspec/roadmap.md` (an entry or "Not placed") with the
      measured reasons; the pull request says what was proven, the Kani
      times, and what was left. Check: `.github/scripts/check-specs.sh`.
