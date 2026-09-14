# Column benchmarks

`column_reader` measures reading and summing selected, non-NULL values from
dense and PostgreSQL Datum storage. It detects overhead relative to an
independent scalar implementation and slowdowns relative to a saved library
revision. `filter_int32` measures scalar comparison filtering over the same two
representations. Neither benchmark measures SQL latency or requires PostgreSQL
or a C compiler. Comparing with the original pg_batch C kernel is a separate
development check, not part of `cargo bench`.

## Running

Run on an idle machine. A full run takes several minutes.

```sh
# Short diagnostic run: 14 cases, with the full sampling method.
cargo bench -p tessera-capi --bench column_reader --locked -- --quick
# Complete check: all 36 cases.
cargo bench -p tessera-capi --bench column_reader --locked
# Save a complete passing run; existing files are never overwritten.
cargo bench -p tessera-capi --bench column_reader --locked -- --save-baseline before
# After a library change, on the same machine and toolchain:
cargo bench -p tessera-capi --bench column_reader --locked -- --baseline before
# Compare only quick dense cases with that complete saved run.
cargo bench -p tessera-capi --bench column_reader --locked -- --quick --filter dense/ --baseline before
# Keep raw diagnostic timings even if the report is FAIL or UNSTABLE.
cargo bench -p tessera-capi --bench column_reader --locked -- --quick --save-results investigation

# Filter diagnostics (14 cases) or the complete check (48 cases).
cargo bench -p tessera-capi --bench filter_int32 --locked -- --quick
cargo bench -p tessera-capi --bench filter_int32 --locked
# Filter baselines have a separate namespace from reader baselines.
cargo bench -p tessera-capi --bench filter_int32 --locked -- --save-baseline before
cargo bench -p tessera-capi --bench filter_int32 --locked -- --baseline before
# Preserve this run regardless of status; also create a baseline if it passes.
cargo bench -p tessera-capi --bench filter_int32 --locked -- --save-results investigation --save-baseline after
```

`--quick` and `--filter TEXT` select diagnostic cases; combining them takes
their intersection. Diagnostic runs are explicitly marked as incomplete and
cannot use `--save-baseline`, even if a filter happens to match every case.
Both may compare selected cases with a compatible, complete saved run.
`--save-results NAME` works with either mode and with `--baseline`; it does not
change the exit status or make diagnostic/unsuccessful results into a baseline.
Use `--help` for options.

## Reader coverage and measured paths

The full matrix has 18 inputs, each read through both storage representations:
selection density, NULLs, partial readiness, word and offset byte masks, and
sizes 0, 1, 63, 64, 65, and 1024. Small sizes exercise boundary costs; these
performance measurements do not replace correctness tests.

The quick subset keeps seven existing inputs at 1024 rows, for 14 cases:

- Full selection with no NULLs, mixed NULLs, or only NULLs.
- Sparse and empty selections without NULLs.
- Full selection with mixed NULLs and partial readiness, using word masks.
- Sparse selection with mixed NULLs and partial readiness, using byte masks
  with bit offset 7.

Every case measures three paths: bulk `Iterator::fold`, short-circuiting
`try_fold`, and per-word reading. These exercise different implementations;
equivalent source sums can produce different machine code. All use
`if let Some(value)` to sum non-NULL values. The per-word API additionally
validates copied selection bounds and padding; that cost is included.

The scalar reference reads the same value and NULL-flag buffers as the library,
without Tessera readers or bitmap operations. Constructors borrow initialized
buffers without copies; no separate reference allocation changes their addresses.
Its reference-versus-itself control checks measurement stability,
not another library implementation. Constructors and allocations are excluded
from timing. All timed entry points receive the same prebuilt input, with one
`black_box` on its reference and one on the result per invocation. Results are
checked against an independent scalar fixture model before timing. Readiness
is checked once per nonempty word.

## Filter coverage and measured paths

The full filter matrix contains 24 inputs, each in dense and Datum form:

- Sizes 65 and 1024, full/every-eighth/one-per-128/empty selections, each
  without NULLs and with mixed NULLs. Byte masks have bit offset 0 (32 cases).
- Sizes 0, 1, 63 and 64, full selection with mixed NULLs and byte masks at
  offset 0 (8 cases).
- Size 1024 with only NULLs in word masks; partial readiness and mixed NULLs
  with full selection in word masks; partial readiness and mixed NULLs with
  full or one-per-128 selection in byte masks at offset 7 (8 cases).

With partial readiness, each selection is intersected with ready rows before
timing. Values deterministically span -50 through 50. The measured operation
is `Gt` with scalar 0. Correctness tests additionally cover all six comparisons,
integer extremes, repeated filtering, and word-local readiness errors.

The quick subset contains both representations of seven inputs: size 65 with
one-per-128 selection, without/with mixed NULLs; size 1024 with full selection,
without/with mixed NULLs; size 1024 with one-per-128 selection and mixed NULLs;
size 1024 with an empty selection and no NULLs; and the partially ready,
one-per-128 case with mixed NULLs at byte offset 7.

Every case measures the library's scalar filter and a self-reference control.
The independent Rust reference reads the same value and flag buffers, but
does not use Tessera readers or bitmap operations. It reads NULL bits per row
directly, without decoding NULL words ahead of time. Both implementations
check readiness once per nonempty word and update the selection word by word.

Each invocation receives its own original selection. Storage is bounded to
4096 masks regardless of the calibrated iteration count. Restoring masks,
allocating and validating their views, and checking results happen outside
timing. Timed blocks apply the filter once to each view; both paths use the
same block size and place `black_box` around inputs and results.

## Measurements and saved runs

Both benchmarks and both run modes make three passes over their selected cases.
Each case is warmed up for at least 30 ms before each pass. The first pass
calibrates an iteration count targeting
about 10 ms per timed segment; the count stays fixed for all three passes.
Each pass collects 15 samples for every measured path and the self-reference control.
Each sample runs reference, library (or reference for the control), reference,
with path order rotated. Its ratio uses the mean of the surrounding reference
times. No samples are discarded and there are no retries until PASS.

The report gives median times, the median of all 45 paired ratios, and the
minimum/maximum of the three pass medians. This range describes observed
repeatability, not a confidence interval. Reader checks remain strictly relative:

- `PASS`: the entire ratio range is at most 1.03.
- `FAIL`: the entire range is above 1.03.
- `UNSTABLE`: the range crosses the limit, or the self-reference control range
  falls outside 0.97–1.03. A bad control invalidates all readers in that case.

Reference overhead and change from a saved run are reported separately. The
relative change range is current minimum / previous maximum
through current maximum / previous minimum; its point estimate is the ratio
of the overall medians. With a valid control, a confirmed `FAIL` takes
precedence over uncertainty in the other comparison. Any `FAIL` or `UNSTABLE`
returns a nonzero exit status and prevents saving a baseline, not raw results.

For filters, a slowdown is significant only when it exceeds **both 3% and
1 ns per call**. Equality passes. Each sample's margin is measured time minus
the larger of `expected * 1.03` and `expected + 1 ns`. The minimum/maximum of
the three series' median margins determine status: `PASS` if the whole range
is at most zero, `FAIL` if it is above zero, otherwise `UNSTABLE`.

Against the reference, expected time is the surrounding reference mean.
Against a saved run, it is that mean multiplied by the old library/reference
ratio: this normalizes absolute differences to current reference speed. Old
maximum/minimum ratios give conservative lower/upper margins. The report also
shows the median paired difference in ns (using the old median for history).
Filter controls still use the strict 0.97–1.03 range, without a 1 ns allowance;
a bad control invalidates the case. No threshold is relaxed to obtain PASS.

`--save-baseline` writes to `target/column-reader-baselines` or
`target/filter-int32-baselines`. Only a full PASS, including any comparison with
`--baseline`, may become a baseline. `--save-results` instead writes to
`target/column-reader-results` or `target/filter-int32-results`, even after FAIL
or UNSTABLE and for diagnostic runs. With both save flags, an unsuccessful run
creates only the results file and still exits nonzero.

Both file kinds use the same TSV encoding, with distinct, versionless headers.
Files store all unrounded before/library/after timings, including controls,
with case, path, pass and sample indices, environment metadata, full/diagnostic
mode, and the name of the previous baseline if used. Results cannot be loaded
as baselines, even if every measurement passed. Saving happens only after all
selected cases finish, before the report; interrupted or invalid collections
are not saved. There is no automatic saving or separate report-replay command.

Files are not committed and are never overwritten. Incompatible files,
including earlier formats, are rejected, not migrated.
CPU, OS, compiler, Rust flags, benchmark sources, Cargo manifests, lockfile,
and the full baseline case set must match. Changes to benchmark sources, even comments
or case selection code, invalidate old runs; use a new name for a new baseline.
In particular, sharing the reader's buffers changes measurement conditions;
old timings cannot establish whether this benchmark refactor improved speed.

The dense reader's fixed modes previously improved NULL-heavy and sparse
reading, with an accepted cost of roughly 1 ns for the measured empty-selection
case on the development machine. This tradeoff does not waive the 3% checks
or create a passing baseline. Quick runs retain that empty-selection case.
Machine load can still affect results; these checks do not guarantee SQL
performance.

## Building a benchmark

Each executable selects its definition, cases, measurement function and source
files, then calls `runner::run`. The source list, together with shared sources,
forms the compatibility fingerprint for saved runs.

The small modules in `support/` have separate responsibilities:

- `runner`: CLI selection, complete-series traversal and saved-run workflow.
- `sampling`: warmup, one-time calibration, rotated reference/library/reference
  segments and raw sample collection.
- `measurement`: pure calculations and acceptance rules, without clock or I/O.
- `report`: controls and comparisons, writing to stdout or a test buffer.
- `baseline`: shared results/baseline persistence and compatibility checks.
- `options`: shared command-line parsing, help and diagnostic-run restrictions.
- `fixture`: shared initialized buffers, masks and borrowed column construction.
- `reading` / `filtering`: their own cases, quick subsets, result checks and
  measured operations. `reference` supplies independent sums and bitmap access.
- `filter_timing`: bounded mask restoration and timed filter blocks.

The runner selects cases once, keeping each case's calibration and samples
together across passes. A measurement function validates one case, then asks
its `sampling::Series` context to collect timings. Its callback selects a path
and times a whole segment: shared
dispatch stays outside the timer, with no added indirect call per operation.
Mutating operations must prepare fresh inputs outside the clock, as the filter
does. Keep reference loops independent of the library under test.

To add or change cases, edit the owning operation module and its coverage tests;
the runner, CLI and report need no changes. Saved runs must then be recreated.
`tests/benchmark.rs` checks both configurations through shared tests for CLI and
persistence, plus synthetic reports, timing, scheduling, reader and filter checks.
Library correctness and Miri tests cover uninitialized gaps; benchmark buffers
are fully initialized.
