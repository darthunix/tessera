# Column benchmarks

`column_reader` measures reading and summing selected, non-NULL values from
dense and PostgreSQL Datum storage. `filter_int32` measures scalar comparison
filtering over the same representations. Neither measures SQL latency or
requires PostgreSQL or a C compiler.

Criterion measures performance; `tessera-bench` compares source revisions
and checks for regressions. Independent scalar references show library overhead.

## Running

Run from the repository root on an idle machine:

```sh
cargo bench -p tessera-capi --bench column_reader --locked
cargo bench -p tessera-capi --bench filter_int32 --locked
# Check correctness without timing.
cargo test -p tessera-capi --benches --locked
```

To check changes against a compatible Git revision:

```sh
cargo run --locked -p tessera-bench -- --base REF
# Check repeatability with identical sources on both sides.
cargo run --locked -p tessera-bench -- --base WORKTREE
```

The candidate defaults to the current working tree, including uncommitted
and new nonignored files. Use `--candidate REF` to select another revision.
The utility does not change your branch or index.

Both benchmarks run by default. `--bench column_reader` or `--bench filter_int32`
selects one. `--filter REGEX` narrows the comparison to selected cases; it must
retain a library operation and its reference. Such a result covers only those
cases. See `cargo run -p tessera-bench -- --help` for options.

A direct `cargo bench` run does not apply the regression checks below.

## What is measured

Cases cover different selection densities, NULLs, readiness, mask layouts
and batch sizes. Library operations and references use the same input buffers.
Setup, allocation and correctness checks are outside timing. Each filter
invocation starts with a fresh selection mask.

## Regression checks

Both revisions must have matching benchmarks, dependencies and build settings.
All binaries are built before timing. Each benchmark runs in
before/after/after/before order, without concurrent measurements or automatic
retries.

Each library operation is checked separately using Criterion's mean estimates
and confidence intervals:

- Repeated runs, including references, must agree within 3%.
- Reading allows at most 3% slowdown.
- Filtering fails only when slowdown exceeds both 3% and 1 ns per call.
- Reference overhead is informational and does not determine pass or fail.

Exit status is 0 when all selected operations pass, 1 for any confirmed
regression, and 2 for uncertainty or an invalid/incomplete run. `UNSTABLE`
is inconclusive, not a successful check.

Reports, raw measurements, logs and source snapshots are saved in a new
`target/bench-runs/compare-*` directory for each comparison.
