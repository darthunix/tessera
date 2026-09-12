# Column reader benchmarks

`column_reader` measures reading and summing selected, non-NULL values from
dense and PostgreSQL Datum storage. It detects overhead relative to an
independent scalar implementation and slowdowns relative to a saved library
revision. It does not measure filtering or SQL latency.

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
```

`--quick` and `--filter TEXT` select diagnostic cases; combining them takes
their intersection. Diagnostic runs are explicitly marked as incomplete and
cannot use `--save-baseline`, even if a filter happens to match every case.
Both may compare selected cases with a compatible, complete saved run.
Use `--help` for options.

## Coverage and measured paths

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

The scalar reference reads buffers and masks without Tessera readers or bitmap
operations. Its reference-versus-itself control checks measurement stability,
not another library implementation. Constructors and allocations are excluded
from timing. All timed entry points receive the same prebuilt input, with one
`black_box` on its reference and one on the result per invocation. Results are
checked against an independent scalar fixture model before timing. Readiness
is checked once per nonempty word.

## Measurements and saved runs

Both run modes make three passes over their selected cases. Each case is warmed
up before each pass. The first pass calibrates an iteration count targeting
about 10 ms per timed segment; the count stays fixed for all three passes.
Each pass collects 15 samples for every reader and the self-reference control.
Each sample runs reference, reader (or reference for the control), reference,
with path order rotated. Its ratio uses the mean of the surrounding reference
times. No samples are discarded and there are no retries until PASS.

The report gives median times, the median of all 45 paired ratios, and the
minimum/maximum of the three pass medians. This range describes observed
repeatability, not a confidence interval:

- `PASS`: the entire ratio range is at most 1.03.
- `FAIL`: the entire range is above 1.03.
- `UNSTABLE`: the range crosses the limit, or the self-reference control range
  falls outside 0.97–1.03. A bad control invalidates all readers in that case.

Reference overhead and change from a saved run are reported separately, both
with the same 3% limit. The change range is current minimum / previous maximum
through current maximum / previous minimum; its point estimate is the ratio
of the overall medians. With a valid control, a confirmed `FAIL` takes
precedence over uncertainty in the other comparison. Any `FAIL` or `UNSTABLE`
returns a nonzero exit status and prevents saving.

Saved runs live in `target/column-reader-baselines` and are not committed.
Format v2 stores all unrounded before/reader/after timings, including controls,
with case, path, pass and sample indices, and environment metadata. Only a full
PASS may be saved. Existing files are never overwritten, and v1 is rejected.
CPU, OS, compiler, Rust flags, benchmark sources, Cargo manifests, lockfile,
and the full case set must match. Changes to benchmark sources, even comments
or case selection code, invalidate old runs; use a new name for a new baseline.

The dense reader's fixed modes previously improved NULL-heavy and sparse
reading, with an accepted cost of roughly 1 ns for the measured empty-selection
case on the development machine. This tradeoff does not waive the 3% checks
or create a passing baseline. Quick runs retain that empty-selection case.
Machine load can still affect results; these checks do not guarantee SQL
performance.

## Supporting code

The files in `support/` implement one benchmark target, not separate benchmarks:

- `fixture`: deterministic inputs and expected sums, built outside timing.
- `reading`: measured reader kernels and uniform reference entry points.
- `reference`: independent scalar reading and bitmap decoding.
- `measurement`: raw samples, summaries, and comparison status.
- `baseline`: persistence and compatibility checks for saved measurements.
- `options`: command-line parsing and diagnostic-run restrictions.

`tests/benchmark.rs` tests this machinery with synthetic timings and checks
reader sums without relying on clock noise. Benchmark buffers are fully
initialized; library correctness and Miri tests cover uninitialized gaps.
