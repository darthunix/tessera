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

The benchmarks run locally on macOS with Apple silicon; Linux and CI are
not planned. Counters need root. Run from the repository root:

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

The utility itself does not run as root. It starts every benchmark process
through `sudo -n`, which never prompts, so the credentials must already be
cached by `sudo -v`. The cache lasts five minutes by default and every
benchmark process refreshes it, so a run of any length needs one `sudo -v`
shortly before it starts. The utility checks `sudo -n -l` before building
and lists the operations of each built executable through `sudo -n` before
measuring, so a missing credential stops the run before the first
measurement. If a run stopped anyway, its `report.txt` and the logs of the
finished cases are in the run directory, and the failing case can be
repeated with `--filter` (see below).

### Without `sudo -v`

Optionally, `sudoers` can allow the benchmark executables without a password.
This is a local choice for one user on one machine; the repository ships no
sudoers files. Note that anything placed at these paths then runs as root
without a password, which is the same trust as a cached `sudo -v` on a
single-user machine and more than that on a shared one. Wildcards in sudoers
do not match `/`, so every directory level is spelled out:

```sh
sudo visudo -f /etc/sudoers.d/tessera-bench
```

```text
# Benchmarks built by tessera-bench from source snapshots and by cargo bench.
USER ALL=(root) NOPASSWD: /path/to/tessera/target/bench-runs/*/*/target/release/deps/column_reader-*, \
  /path/to/tessera/target/bench-runs/*/*/target/release/deps/filter_int32-*, \
  /path/to/tessera/target/release/deps/column_reader-*, \
  /path/to/tessera/target/release/deps/filter_int32-*
```

With such an entry `sudo -n -l` succeeds without a password and the utility
runs without `sudo -v`.

### What the unprivileged tests cover

`cargo test -p tessera-bench` runs the measurement pipeline against a
stand-in benchmark script: interleaving of the processes, the run
directories and logs, the statuses and exit codes for instruction and cycle
regressions and for zero readings, and a failing benchmark process. The
benchmark programs' own tests cover `--list`, the check mode and the block
bookkeeping. Calibration, warm-up and the repetition of zero readings in
the runner run only with counters, that is, only as root.

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

### Adding an operation

An operation is registered on a case's group with `Group::op(name, closure)`
in the `measure_*` function of its module; the closure must return a value,
which the runner black-boxes, and must not allocate or check results (do
that once before registering, as `measure_column` does). Names are free:
tessera-bench treats every operation as a library path except `reference`,
and every group must register a `reference`. A new operation, like any
change under `benches/`, makes the benchmark incompatible with earlier
revisions: commit it first, then compare later changes against that commit.

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
  operation of at least 500 cycles, and the median confirms it by growing
  more than 3%. This catches slowdowns that instructions cannot see, such as
  a longer dependency chain on the accumulator. The minimum over the blocks
  of three processes stayed within 3% between identical binaries on long
  operations, bistable ones included.
- `WARNING cycles` when the minimum cycles per call grow by more than 3%, or
  by more than 4 cycles for operations under 200 cycles, where a percentage
  is a fraction of a cycle, and the median grows by as much. A real slowdown
  moves the whole distribution; the minimum alone is one lucky block, and
  identical binaries have shown it 3-17% apart with medians within 1.5%.
  Printed with the branch-miss change; layout, predictor behaviour and
  dependency chains need a manual look.
- `MODES` when the medians of the processes, or the blocks of one process,
  differ by more than 1.10x and by more than 10 cycles: the operation has
  more than one cost depending on core state. That is a property of the code
  worth fixing; the minimum still compares its best mode. A few cycles
  between modes of a 25-cycle operation are not worth a flag.
- `SLOWER-WITH-FEWER-INSTRUCTIONS` marks the class of changes that save
  instructions and lose cycles.
- `PASS` otherwise.

Every line also shows the median cycles, the modes ratio, branch misses per
call and the cores the blocks ran on. References show library overhead as
instruction and cycle ratios and do not affect statuses. `PER ROW` divides
instructions and minimum cycles by the case's row count (the first numeric
segment of the id) and shows instructions per cycle in the best mode; for
cases without rows only the IPC is shown. These rates are for judging the
code, not the change. Exit status is 0 when every selected operation passes
(warnings included), 1 for any regression, and 2 for inconsistent counting
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
