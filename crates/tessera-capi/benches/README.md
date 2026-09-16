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
# One process per side when only instructions matter (about 35 seconds).
cargo run --locked -p tessera-bench -- --base REF --repeats 1
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
benchmarks, dependencies and build settings. Each case is measured by
`--repeats` (default 3) pairs of processes, interleaved as A1 B1 A2 B2 A3 B3,
so that drift in core state is shared by both sides. Instructions are
deterministic and would need one process; cycles are not: a process inherits
the predictor state of the cores it runs on, and identical binaries have
shown whole processes 30% apart in cycles at identical instruction counts.
Several processes give several states, and the minimum over all their
blocks is the operation's cost in the best state seen. The full set runs
in about a hundred seconds.

## Reading the report

Each library operation gets its own status. The limits are constants at the
top of [report.rs](../../../tools/tessera-bench/src/report.rs):

- Instructions per call are the median over all blocks of a side. Blocks of
  one process and processes of one side must agree within 0.1%, and no block
  may read zero; otherwise the operation is `UNSTABLE`, which means counting
  itself is broken, not that the code is noisy.
- `FAIL` when B needs more than 1% more instructions per call than A.
- `FAIL cycles` when the minimum cycles per call grow by more than 10% on an
  operation of at least 500 cycles that runs in a single mode on both sides.
  This catches slowdowns that instructions cannot see, such as a longer
  dependency chain on the accumulator.
- `WARNING cycles` when the minimum cycles per call grow by more than 3%, or
  by more than 4 cycles for operations under 200 cycles, where a percentage
  is a fraction of a cycle. Printed with the branch-miss change; layout,
  predictor behaviour and dependency chains need a manual look.
- `MODES` when the medians of the processes, or the blocks of one process,
  differ by more than 1.10x: the operation has more than one cost depending
  on core state. That is a property of the code worth fixing, but cycles do
  not fail such an operation because the comparison would be a coin toss.
- `SLOWER-WITH-FEWER-INSTRUCTIONS` marks the class of changes that save
  instructions and lose cycles.
- `PASS` otherwise.

Every line also shows the median cycles, the modes ratio, branch misses per
call and the cores the blocks ran on. References show library overhead as
instruction and cycle ratios and do not affect statuses. Exit status is 0
when every selected operation passes (warnings included), 1 for any
regression, and 2 for inconsistent counting or an invalid/incomplete run.

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
