## Why

The benchmark programs of `crates/tessera-capi/benches` call each
operation on the same inputs, call after call. A case is built once,
from a generator with a fixed seed, and a block of the runner repeats
the call thousands of times. The branch predictors of both machines
learn those inputs, even the cases that are there to show the cost of
a branch that depends on the data:

- The `random` selection and the `nulls-random` flags. The loop over
  the selected rows of a word ends at a different row in every word,
  yet the fold of `column_reader` over them misses 0.03 branches a call
  of 1024 rows on the Intel Mac, where a predictor that had not seen
  the rows would miss about one a word, 16 a call. A branch on the NULL
  flag of each slot, tried as a candidate, missed 0.004 times a call
  over 30 % NULL rows at random places.
- The `shuffled` values of `set_int32`. The branch of a search by
  halving on x86-64 misses 12 to 15 times in about 19 750 a call, as
  with values in order, where half of its steps would miss on values
  it had not seen.
- The int8 division by a scalar over random words. After its code
  moved by 96 bytes, unchanged, it mispredicted 17 branches a call
  where it had 1 or 2, and took 12 to 14 % more cycles: what the
  predictor learns depends on where the code lies.

So three kinds of verdicts cannot be trusted. A branch cannot be
judged against a choice made as data: the branch wins on inputs the
predictor has learned and may lose on any other. The paths that read
rows one by one, full of such branches, look cheaper than they are,
and every bound between whole words and rows was measured against
them. And a FAIL of cycles can come from where the code lies rather
than from what it does.

This change takes the first of the findings that the close of
`simd-primitives-avx2` left without a decision. The maintainer took
the six findings on 2026-11-01, in three changes; this one comes
first, because the second, `whole-word-decisions`, measures with it.

## What Changes

- The cases that are drawn at random get inputs that the predictor
  cannot learn: every call meets rows, NULL flags or values it has not
  met in the calls before it within reach of the predictor's memory.
  The work of a call stays: every input of a case has the same row
  count and, in each word, the same numbers of selected and NULL rows,
  at positions drawn anew.
- The form is chosen by a pilot on both machines, among three: a ring
  of inputs drawn from different seeds and taken in turn; one input
  long enough that its branches do not repeat; a fresh input before
  every call, outside the counted calls.
- The criterion of the pilot: the misses a call reach those of a
  predictor that has not seen the inputs, computed from the case
  itself, and stop growing when the inputs grow more varied, while a
  control of the same footprint over one input does not; blocks keep
  the same instructions, within the 0.1 % that `tessera-bench`
  allows.
- The cases with a regular pattern (`all`, `half`, `eighth`,
  `sparse`, `empty`, `nulls-mixed`) stay as they are: they show the
  cost of a call whose branches the predictor knows.
- The README of the benchmarks and `docs/measuring.md` say which cases
  the predictor learns and which it cannot.
- A run of every program against itself on both machines, as root on
  the Intel Mac and on the M5 Pro, reports no FAIL and no UNSTABLE on
  the new cases.

Not in this change:

- The decisions measured again with the new cases: they are
  `whole-word-decisions`.
- The prefetch hint of `relhash`, which is about caches, not
  branches: `relhash-prefetch-by-size`.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

None. The change touches benchmark programs and their documents only.
No kernel, entry point, format, behavior seen from SQL or public API
of a crate changes. The change declares `skip_specs: true`.

## Impact

- Code: the support of the benchmark programs,
  `crates/tessera-capi/benches/support/` (the fixtures, the runner and
  the cases of reading and of the sets), and their tests in
  `crates/tessera-capi/tests/benchmark.rs`. `tools/tessera-bench` only
  if the chosen form needs the tool to know of it.
- Documents: `crates/tessera-capi/benches/README.md`,
  `docs/measuring.md`.
- Runs: the new cases change the benchmarks, and `tessera-bench`
  compares only sides with the same benchmarks, so a base before this
  change cannot be compared with one after it on those cases.
- No production code, header, setting, SQL suite, format or
  dependency changes.
- The roadmap: the finding leaves "Not placed" for this change's
  entry; at the close the entry leaves.
