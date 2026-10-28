## Why

Tessera has vector code for full words of 64 rows only on AArch64,
with NEON. On x86-64 every such word takes the row path, and x86-64 is
the target platform: Linux on x86-64, by the maintainer's decision of
2026-10-05. The NEON code paid off on an Apple M5 Pro, measured
against the row path in cycles:

- filters where every row of a word is selected: 64 to 85 % fewer;
- arithmetic: 67 to 84 % fewer, division by a scalar 20 to 47 %;
- `sum`, `min`, `max` and `count` over such words: 40 to 89 % fewer;
- hashes of one key and of two keys: 82 and 83 % fewer on dense
  columns, 69 to 73 % on Datum columns.

On x86-64 none of it runs. `crates/tessera-kernels/src/simd/` is
compiled only for AArch64. Off AArch64, 21 functions are stubs that
panic if called: 18 hooks of the int4 and int8 lanes, the sum of int4
words, `count` and `cast`. The sets find no hits there instead. A
`cfg!` in each driver keeps every word on the row path.

The maintainer's Intel Mac is at hand: a Core i5-1038NG7 (Ice Lake,
four cores, eight threads, AVX2 and AVX-512). Since the change
`counters-on-x86-and-linux`, `pmu` counts on it as root. Two things
are missing before AVX2 kernels can be written and accepted here:

- `tessera-bench` has never run on Intel. Its limits (1 % of
  instructions, 3 and 10 % of cycles) were set by the spread of
  identical binaries on the M5 Pro. Turbo and a second thread on each
  core may spread cycles further on this machine.
- The NEON code is to be written once over a small set of primitives
  that both vector sets implement, by the maintainer's decision of
  2026-09-25. The condition was that this costs NEON nothing, and the
  machine code of AArch64 shows it. `tessera-bench --disasm` builds
  only for the machine it runs on, so on this Mac it cannot show NEON
  code.

The change takes up the roadmap's entry `simd-primitives-avx2`. On
2026-10-26 the maintainer took it now, on the Intel Mac, ahead of
`explain-rows-per-loop` and `join-pruning-cost`. Of the entry
`bench-in-four-cells`, which it depended on, it takes only the run of
`tessera-bench` on the Intel Mac.

## What Changes

- `tessera-bench` on the Intel Mac. A run of identical sources on both
  sides as root must report no FAIL; its spread of cycles is recorded.
  The texts that say the counters work only on Apple silicon then say
  macOS on Apple silicon and on Intel.
- `tessera-bench --disasm --target <triple>` builds both sides for
  another target. On this Mac it compares the NEON code of
  `aarch64-apple-darwin`.
- `--disasm` marks a function whose instructions differ only by a
  consistent renaming of registers `(registers only)`, as it marks
  `(immediates only)` today.
- A trait of primitives over a vector set, with about fifteen
  operations of one to five intrinsics, implemented for NEON and for
  AVX2. The kernels of `simd/` are written once over it.
- First a pilot: the int4 filter and the comparison of two columns
  over the primitives, on both sets.
  - The pilot holds when the AArch64 code of every Tessera function in
    the benchmark programs is the same as before, up to registers, and
    the AVX2 code of a word has no calls.
  - If it does not hold, the maintainer is asked first. The fallback,
    the decision of 2026-09-20, is two parallel implementations: the
    NEON code unchanged and AVX2 in `simd/avx2/`.
- Then each kernel in turn: the filter of int8, the arithmetic of int4
  with its divisor, of int8, the aggregates and `count`, the widening
  cast, the hashes, the sets. For each, the NEON code moves with the
  same check, and AVX2 is added and accepted by its counters on this
  Mac, operation by operation: an operation whose AVX2 code does not
  gain keeps the row path on x86-64.
- On x86-64 the vector path is chosen at run time by
  `is_x86_feature_detected!("avx2")`, once a call, with no
  `-C target-cpu`. Other targets and Miri keep the row paths.
- The tests that compare the vector path with the row path run on
  x86-64 too. `TESSERA_REQUIRE_SIMD=1` makes a missing vector set an
  error instead of a quiet pass, and CI sets it.
- The mutants of the AVX2 code are judged on an x86-64 runner. The job
  of mutants runs on arm64, where that code is cut out and its mutants
  would survive unseen.
- Four decisions taken by timing on the M5 Pro are measured again on
  Intel:
  - `BULK_MIN_ROWS`;
  - the bounds of `SetValue::WORD_KEYS` and the branch-free compares
    of the sets;
  - the counted loop of a full word in `tessera-capi`;
  - the branch-free read of a NULL slot.

  A value that Intel wants otherwise becomes a value per architecture.
- The prefetch hint of `relhash` on x86-64, last. The probe of a batch
  asks the processor to fetch memory early on AArch64 alone; on x86-64
  the same hint is an SSE instruction, which every x86-64 has, so it
  needs no token. It goes into the crate's own copy of the hint, as
  the forecast of the crate's contract says, and is kept on its
  counters on this Mac.

Not in this change:

- AVX-512.
- Linux without `sudo` and the cell in a report, which stay in
  `bench-in-four-cells`.
- The loops that save their values around a kernel's call
  (`sturdy-bulk-loops`).

## Capabilities

### New Capabilities

None.

### Modified Capabilities

None. The change makes no promise on any surface a spec names. The
kernels' entry points, their results and errors stay as they are, and
so do the behavior seen from SQL and every format. No public API of a
crate with `publish = true` changes: `relhash` gains its hint on
x86-64 inside, and `pmu` is not touched. The change declares
`skip_specs: true`.

## Impact

- Code:
  - `crates/tessera-kernels`: `simd/`, the drivers and lanes of `int`,
    `int32` and `int64`, `count`, `cast`, `set`, the crate's
    documentation;
  - `tools/tessera-bench`;
  - `crates/relhash`: `table/lanes.rs`, the prefetch hint;
  - perhaps `crates/tessera-capi`, if a re-measured decision changes.
- `unsafe` stays in `simd/` only. The AVX2 code calls functions
  compiled for AVX2, and only once the processor is known to have it.
- CI: `TESSERA_REQUIRE_SIMD=1` in the Rust jobs. On `ubuntu-24.04`,
  an x86-64 runner with AVX2, the Rust tests, the SQL suites and the
  random queries then run the AVX2 path. The mutants of the lines a
  pull request changes in the AVX2 code run on that runner too.
- Documents:
  - the crate's documentation and that of `simd`;
  - `docs/kernels.md`, where it counts lanes in a vector;
  - `docs/limitations.md`, which says the vector kernels are NEON
    only;
  - `crates/tessera-capi/benches/README.md`, whose link to `pmu`
    points to `crates/pmu`, a folder that is gone;
  - `docs/measuring.md` and "Accepted limits" in `CONTRIBUTING.md`,
    for the Intel Mac;
  - the design of `relhash` and the forecast of its contract, in
    `table-crate`, for the hint on x86-64.
- No public header, setting, SQL suite, format or dependency changes.
- The roadmap at the close:
  - this entry leaves;
  - `bench-in-four-cells` loses the run on the Intel Mac and its texts;
  - `linux-x86-support` no longer waits for vector kernels.
