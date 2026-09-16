# Column benchmarks

There are two benchmark programs:

- [column_reader](column_reader.rs) reads and sums selected, non-NULL values
  from dense and PostgreSQL Datum storage.
- [filter_int32](filter_int32.rs) filters values from the same representations.

Neither measures SQL latency or requires PostgreSQL. They measure PMU
counters, not time: retired instructions, core cycles, mispredicted branches
and retired branches per call, through [tessera-pmu](../../../tools/tessera-pmu).
The separate [tessera-bench utility](../../../tools/tessera-bench/src/main.rs)
builds and runs them to compare revisions.

## Why counters and not time

Instructions retired are deterministic for a fixed input. They do not depend
on the core a thread runs on, on its frequency, or on branch predictor
state, which is where the wall-time noise of these benchmarks came from:
threads hop between cores about 70 times per second, and a loop's cost then
depends on the predictor state each core has accumulated. Nanosecond-scale
operations have no wall-time resolution at all: 3% of 3 ns is a fraction of
a cycle. Cycles are wall time without frequency scaling, and branch misses
explain most cycle-only differences.

## Running

Counters need root. Run from the repository root:

```sh
# Correctness checks of every case without counters or root.
cargo test -p tessera-capi --benches --locked
# Operation ids, without root.
cargo bench -p tessera-capi --bench column_reader --locked -- --list
# Measure everything (or `--only ID` for selected operations) as root.
cargo bench -p tessera-capi --bench column_reader --no-run --locked
sudo target/release/deps/column_reader-<hash> --output results.jsonl
```

To check changes against a compatible Git revision:

```sh
sudo -v
cargo run --locked -p tessera-bench -- --base REF
# Repeatability with identical sources on both sides.
cargo run --locked -p tessera-bench -- --base WORKTREE
```

The utility starts the benchmark processes through `sudo -n`, so `sudo -v`
must have cached the credentials first; the utility itself does not run as
root and does not change `sudoers`.

## Cases and operations

A case is one input configuration in one storage representation. For example,
this case uses a dense column of 1024 rows, word-based masks, half the rows
selected, a mix of NULL and non-NULL values, and all rows ready to read:

```text
column_reader/dense/words/1024/half/nulls-mixed/ready
```

An operation is the function measured on those inputs. Reading measures a
`fold` sum through `try_fold_selected`, a `words` sum through mask words, and
an independent `reference` sum. Filtering measures the library's `scalar`
filter and a `reference` filter. The reference is a simple separate
implementation of the same task, not another source revision.

The `cases()` functions and measured operations are in
[reading.rs](support/reading.rs) and [filtering.rs](support/filtering.rs).
Their shared [Fixture](support/fixture.rs) creates values and masks. Each input
configuration is used with both `dense` and `datum` storage; these count as
separate cases.

Library operations and references within a case use the same input buffers.
Setup, allocation and correctness checks are outside the counted regions.
Each filter invocation starts with a fresh selection mask.

## How an operation is measured

The [runner](support/runner.rs) calibrates the number of calls per block so
that a block takes about 20 million cycles, warms up with one block, and
then counts ten blocks. The output file has one JSON line per operation with
the raw readings of every block:

```json
{"id":"column_reader/dense/words/1024/all/nulls-none/ready/fold","iters":50000,
 "instructions":[...10 values...],"cycles":[...],"branch_misses":[...],"branches":[...],
 "cpus":[[12,12],[12,15],...],"retries":0}
```

Per-call values are the block readings divided by `iters`; they include the
call loop and the `black_box` that keeps each result alive. `cpus` records
the CPU a block started and ended on, which explains cycle modes: the
scheduler moves the thread between cores, and each core keeps its own
predictor state. A block whose counters read zero is a failed counter read;
it is repeated up to three times per operation and `retries` counts that.
Statistics and limits are computed by tessera-bench, not by the benchmark
programs.

## How revision comparison works

A is the revision selected by `--base`. B is selected by `--candidate`, which
defaults to `WORKTREE`: the current working tree, including uncommitted changes
and new nonignored files. The utility does not change your branch or index.

For each benchmark program, the utility builds an A executable and a B
executable from isolated source copies. Both revisions must have matching
benchmarks, dependencies and build settings. Each case is measured by one A
process and then one B process; instructions are deterministic, so repeated
processes add nothing, and the whole set runs in a couple of minutes.

## Reading the report

Each library operation gets its own status:

- Instructions per call are the median over blocks. Blocks of one process
  must agree within 0.1%; otherwise the operation is `UNSTABLE`, which means
  counting itself is broken, not that the code is noisy.
- `FAIL` when B needs more than 1% more instructions per call than A.
- `WARNING cycles` when the median cycles per call grow by more than 3%;
  it is printed with the branch-miss counts and does not change the exit
  status. A cycles-only change with unchanged instructions is either code
  layout, predictor behaviour or a longer dependency chain, and needs a
  manual look rather than an automatic verdict.
- `PASS` otherwise.

References show library overhead as an instruction ratio and do not affect
statuses. Exit status is 0 when every selected operation passes (warnings
included), 1 for any instruction regression, and 2 for inconsistent counting
or an invalid/incomplete run.

Reports, raw readings, logs and source snapshots are saved in a new
`target/bench-runs/compare-*` directory for each comparison.

## Shorter runs

Use `--bench` to select one program and `--filter SUBSTRING` (repeatable; any
match keeps an operation) to select cases:

```sh
cargo run --locked -p tessera-bench -- --base HEAD \
  --bench column_reader --filter '/1024/half/nulls-mixed/ready/'
```

A filter must retain a library operation and its reference in every selected
case. See `cargo run -p tessera-bench -- --help` for options.
