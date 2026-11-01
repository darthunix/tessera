## Why

A driver of the vector kernels decides, for a call or for each word of
64 rows, whether to take a word whole with vector code or to read its
selected rows one by one. The close of `simd-primitives-avx2` left
five findings about that decision, which no one rule settled:

- **The losses of the row paths of x86-64** (the roadmap's entry
  `x86-row-paths`). Before AVX2 the decision was compiled out on
  x86-64; now it shares the function of the row loops, which run short
  of registers there. A call that reads rows one by one pays: the
  filter of int4 38 cycles more on one dense row in 128, the comparison
  of two columns 224 cycles (4.5 %) on a Datum batch partly prepared,
  the cast and the hashes 12 to 16 instructions a call or a key, the
  sets 19 % more instructions on empty selections. The maintainer
  accepted these for the pilot, to be settled once for every driver.
  With them came two leftovers: the loops of `count` and of the
  aggregates have one shape on x86-64 and another on AArch64, and
  `word_slots` checks its bounds where `as_chunks` would not, six
  instructions of a whole word of `count`.
- **One bound of whole words for every family.** `BULK_MIN_ROWS` is 12
  selected rows on both machines. On the Intel Mac words of 8 rows
  taken whole gain on the filters, the hashes, the cast and `count`,
  15 to 62 % of cycles, and lose on the sums, `min` and `max`, the
  arithmetic by a constant, the compare of Datum columns and the sets,
  up to 2.5 times the cycles.
- **The sets and the words of NULL rows.** The sets compare every lane
  of a whole word, also where all its rows are NULL, which the row path
  skips: 64 int4 keys take 90 to 151 % more cycles there on AVX2.
- **One bound of keys for every word of the sets.** Up to 64 int4 keys
  and 16 int8 keys are compared whole on any word of 12 rows or more,
  whatever its rows. On the Intel Mac, 65 int4 keys compared whole cost
  as much as halving on words of 16 rows and a quarter of it on full
  words; 64 int8 keys cost 2.2 times halving on words of 16 rows and
  half of it on full words.
- **The call of a full word in the reader's fold.** `try_fold_words` in
  `tessera-capi` calls a function of its own for a full word. On
  x86-64, without that call, sparse selections, words of 8 rows and
  calls of a row or none read 5 to 27 % faster, while the full words
  then took 2 to 7 times the cycles.

They are one question: when and where a driver takes a word whole, and
what the choice costs the paths that read rows. Settling them apart
would measure the same drivers several times, and each answer would
move the others.

The maintainer took the six findings on 2026-11-01, in three changes,
with `x86-row-paths` in this one. It comes second, after
`bench-unlearned-inputs`: a bound between whole words and rows weighs
the cost of the row paths, and their branches must be measured on
inputs the predictor has not learned.

## What Changes

- For each family, storage and density, on both machines, what taking
  words whole saves or costs against reading their rows: two builds
  that differ only in the bound, one that takes no word whole and one
  that takes every word whole, measured against each other over cases
  of 8, 12, 16, 24 and 32 selected rows a word, at positions the
  predictor has not learned. Each family's bound of rows follows, a
  value for each architecture where the machines part, and so does
  the bound of keys of the sets, which then grows with the rows of the
  word.
- The sets decide on the rows that are present, not NULL, where the
  storage tells them at the cost of a mask (dense blocks) or of the
  NULL flags they read anyway (Datum blocks). A family that decides on
  its first word does the same where `nulls-all` shows a loss.
- The place of the decision in the drivers on x86-64, chosen by the
  machine code and the counters among forms written down first, so
  that a call that reads rows costs no more than in a control build
  whose vector path the compiler removes, within the noise of the
  benchmark.
- One shape of the loops of `count` and of the aggregates for both
  machines, if one serves both; `as_chunks` in `word_slots`.
- A form of the reader's fold on x86-64 that keeps the gain of full
  words and loses nothing on the other words, if one exists.
- Every candidate: the machine code of both targets, and the counters
  of both machines, two runs each, the base after
  `bench-unlearned-inputs`. A value or a form that one machine wants
  otherwise becomes one for each architecture.

Not in this change:

- The benchmark of unlearned inputs, which it uses:
  `bench-unlearned-inputs`.
- The prefetch hint of `relhash`: `relhash-prefetch-by-size`.
- The loops that keep their values across the call of a kernel
  (`sturdy-bulk-loops`), beyond the reader's fold.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

None. A decision between whole words and rows gives the same results
either way, and the tests compare the two. No entry point, result,
error, format, behavior seen from SQL or public API of a crate
changes. The change declares `skip_specs: true`.

## Impact

- Code:
  - `crates/tessera-kernels`: the drivers of `int/`, `count`, `cast`
    and `set`, the bounds in `lib.rs` and `set.rs`, the documentation
    that gives their reasons;
  - `crates/tessera-capi`: `column.rs` (the fold of selected words)
    and `column/ints.rs` (`word_slots`);
  - the benchmark programs: cases of 8, 12, 16, 24 and 32 selected
    rows a word at positions drawn anew, in the reading fixtures, in
    the matrices of the filters and of the comparisons, and in
    `set_int32`.
- Documents: `docs/kernels.md`, where it gives the bounds.
- No header, setting, SQL suite, format or dependency changes.
- The roadmap: the four findings leave "Not placed" and
  `x86-row-paths` leaves its entry, for this change's entry; at the
  close the entry leaves.
