# Column benchmarks

There are two benchmark programs:

- [column_reader](column_reader.rs) reads and sums selected, non-NULL values
  from dense and PostgreSQL Datum storage.
- [filter_int32](filter_int32.rs) filters values from the same representations.

Neither measures SQL latency or requires PostgreSQL. Criterion measures time
inside these programs. The separate
[tessera-bench utility](../../../tools/tessera-bench/src/main.rs) builds and
runs them to compare revisions; its overhead is not included in the reported
time per operation.

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

A direct `cargo bench` run does not apply the regression checks below.

## Cases and operations

A case is one input configuration in one storage representation. For example,
this case uses a dense column of 1024 rows, word-based masks, half the rows
selected, a mix of NULL and non-NULL values, and all rows ready to read:

```text
column_reader/dense/words/1024/half/nulls-mixed/ready
```

An operation is the function measured on those inputs. Reading measures a
`fold` sum through `try_fold_selected`, a `words` sum through mask words, and
an independent `reference` sum. Filtering measures the library's
`scalar` filter and a `reference` filter. The reference is a simple separate
implementation of the same task, not another source revision.

The `cases()` functions and measured operations are in
[reading.rs](support/reading.rs) and [filtering.rs](support/filtering.rs).
Their shared [Fixture](support/fixture.rs) creates values and masks. Each input
configuration is used with both `dense` and `datum` storage; these count as
separate cases.

Library operations and references within a case use the same input buffers.
Setup, allocation and correctness checks are outside timing. Each filter
invocation starts with a fresh selection mask.

## How revision comparison works

A is the revision selected by `--base`. B is selected by `--candidate`, which
defaults to `WORKTREE`: the current working tree, including uncommitted changes
and new nonignored files. The utility does not change your branch or index.

For each benchmark program, the utility builds an A executable and a B
executable from isolated source copies. It compares reading with reading and
filtering with filtering, not the two programs against each other. When both
sides are `WORKTREE`, they use copies of the same source snapshot: this checks
measurement stability rather than a code change.

Both revisions must have matching benchmarks, dependencies and build settings.
All builds finish before measurements begin. Then each case gets four process
runs in A/B/B/A order. For a full reading case:

```text
Case 1:
  A1: fold -> words -> reference
  B1: fold -> words -> reference
  B2: fold -> words -> reference
  A2: fold -> words -> reference
Case 2: its own complete A/B/B/A sequence, then the next case.
```

By default, cases run one at a time (`--jobs 1`). With `--jobs N` (or `-j N`),
up to N cases share one queue across reading and filtering. Each worker finishes
a case's full A/B/B/A sequence before taking another case. The actual worker
count is capped by the number of selected cases.

Each operation in each run gets its own warm-up and many repeated calls.
Criterion chooses the number of calls and collects timing samples; these are
not just four individual calls. Reported nanoseconds are the estimated time
per call over the case's whole input, not per row or a single stopwatch reading.
The current measurement settings are in [support/mod.rs](support/mod.rs).

The utility's [cases.rs](../../../tools/tessera-bench/src/cases.rs) groups the
operation names reported by Criterion and controls this order; it does not
create column data. There are no automatic retries. On error, workers stop
starting new cases and phases, wait for already running processes, and keep
their logs; an incomplete comparison is not accepted.

## Reading the report

Each library operation gets its own status, not one status per case. For
example, `fold` may pass while `words` on the same inputs is unstable.

First the utility checks repeatability: A1 against A2, and B1 against B2.
Then it compares A with B, using the average of each side's two mean estimates
and bounds that cover both repeats' uncertainty. A difference between the
printed averages alone does not determine the result.

The limits are:

- Repeatability allows a difference of up to 3% in either direction;
  filtering also allows up to 1 ns per call in either direction.
- Reading allows at most 3% slowdown.
- Filtering fails only when slowdown exceeds both 3% and 1 ns per call.

`PASS` means repeatability passed and slowdown stays within the limits after
accounting for uncertainty. `FAIL` means repeatability passed but slowdown
clearly exceeds the limits. `UNSTABLE` means repeats disagree too much or the
uncertainty is too large to decide.

Reference timings show library overhead, but do not determine these statuses
or adjust the A/B comparison. Reference repeats outside ±3% produce warnings;
missing or invalid results remain errors. The calculation is in
[report.rs](../../../tools/tessera-bench/src/report.rs).

Exit status is 0 when all selected operations pass, 1 for any confirmed
regression, and 2 for uncertainty or an invalid/incomplete run. `UNSTABLE`
is inconclusive, not a successful check.

Reports, raw measurements, logs and source snapshots are saved in a new
`target/bench-runs/compare-*` directory for each comparison.
`execution.json` records requested and actual parallelism and Criterion's
thread count. `timings.json` records preparation, measurements and total time
inside the utility, including benchmark builds but not compilation of the
utility itself. Final report ordering does not depend on completion order.

## Shorter runs

Both benchmark programs and all their cases run by default. Every selected
operation is measured four times, so a rough timing budget is:

```text
selected operations (including references) x 4 x (warm-up + measurement time)
```

Builds, analysis and process startup take additional time. To run the full set
with up to eight cases at once:

```sh
cargo run --locked -p tessera-bench -- --base WORKTREE --jobs 8
```

Parallel runs may be faster, but do not isolate CPU cores: cases still compete
for shared resources and can affect each other's timings. The utility does not
pin processes to cores. It sets `RAYON_NUM_THREADS=1` for every benchmark child,
even with `--jobs 1`, so Criterion's statistical analysis does not add another
layer of parallel work. Compare stability as well as elapsed time; there is no
guaranteed speedup.

To do less work, use `--bench` to select one program and `--filter REGEX` to
select cases.
For example:

```sh
# Reading: half-selected inputs with mixed NULLs, in both representations.
cargo run --locked -p tessera-bench -- --base HEAD \
  --bench column_reader --filter '/1024/half/nulls-mixed/ready/'
# Filtering: fully selected inputs without NULLs, in both representations.
cargo run --locked -p tessera-bench -- --base HEAD \
  --bench filter_int32 --filter '/1024/all/nulls-none/ready/'
```

A filter must retain a library operation and its reference in every selected
case. These examples keep all operations of the matching cases. They run fewer
cases with the same measurement settings: a pass covers only those cases,
not the full benchmark set. See `cargo run -p tessera-bench -- --help` for options.

Shortening measurements is a different tradeoff: it can make them less robust
against temporary system load. See
[Criterion's measurement-time documentation][measurement-time].

[measurement-time]:
  https://docs.rs/criterion/0.8.2/criterion/struct.Criterion.html#method.measurement_time
