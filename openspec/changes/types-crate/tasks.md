## 0. How the work goes

The branch `types-crate` starts from `main`, since the crate needs
nothing of `source-scan`; `parquet-fdw`, which needs both, carries this
change's commit on its own branch until this one is merged. Every task
is one commit, reviewed by Codex and Claude Fable before the next
starts. Each commit passes `make rust-check`,
`.github/scripts/check-specs.sh` and the check named in its task.

## 1. The proposal

- [x] 1.1 Write the proposal, the spec delta, the design, the tasks and
      the contract of this change; in the roadmap, the entry
      `types-crate` taken, and `parquet-fdw` depending on it. Check:
      `openspec validate types-crate --strict`;
      `.github/scripts/check-specs.sh`

## 2. The crate

- [x] 2.1 `crates/datum-arrow`, `publish = true`, depending on
      `arrow-schema` alone: the PostgreSQL types it knows, the Arrow
      types it takes, the kinds of failure with their SQLSTATEs, its
      rustdoc; the job "datum-arrow alone" in CI, as `relhash`'s: its
      tests alone, its documentation with warnings denied, `cargo
      publish --dry-run`, and the diff of its public API with the
      snapshot `openspec/changes/types-crate/public-api.txt`, which the
      close moves beside the spec. Check: `make rust-check`; `cargo
      package -p datum-arrow`
- [x] 2.2 Read plans and the conversions of values read, with unit tests
      at the edges of every range and property tests over every bit
      pattern; the hot loops without a branch per value, their machine
      code read on AArch64 and x86-64. Check: `make rust-check`; `cargo
      rustc --release -p datum-arrow --lib -- --emit asm` on both
      targets, its loops kept in the pull request's Performance item
- [x] 2.3 Write plans and the conversions of values written, with the
      property test of the round trip and the test against Arrow's own
      arrays through `arrow-array`. Check: `make rust-check`
- [x] 2.4 The bounds of a comparison in a file's unit, with the property
      test against an exact count in 128 bits of each stored value,
      those the conversion refuses as out of range among them. Check:
      `make rust-check`
- [x] 2.5 The Kani proofs of the code of one value (bounds of integers,
      days and microseconds, ranges, days and times, the failures of
      plans of integers, short days and seconds, the zero byte and the
      characters of short strings) and the crate's row in the Kani job
      of CI. Check: `cargo kani -p
      datum-arrow`
- [x] 2.6 The crate's mutants all caught by its tests, or named, as the
      mutants job of a pull request judges them. Check: `cargo mutants
      -p datum-arrow`

## 3. The spec, the design and the contract

- [ ] 3.1 The rules and lists of the spec delta move into the crate's
      documentation, and each requirement links to its items; every
      scenario names its test, and every pending check of the contract
      its check. Check: `openspec validate types-crate --strict`;
      `.github/scripts/check-specs.sh`
- [ ] 3.2 `openspec/specs/datum-arrow/design.md`, written for a reader
      who knows nothing of the crate; `## Files` and `## Tests` last.
      Check: `.github/scripts/check-specs.sh`
- [ ] 3.3 The pull request's description, an index of evidence. Check:
      CI green

## 4. The close

- [ ] 4.1 `openspec archive types-crate --yes`, the folder it makes
      under `openspec/changes/archive/` deleted, the contract and the
      snapshot of the public API moved beside the spec, the job of CI
      reading it there; the entry `types-crate` leaves the roadmap.
      Check: `.github/scripts/check-specs.sh`
