# tessera-pmu

Per-thread PMU counters for benchmarking arbitrary code: retired
instructions, core cycles, mispredicted branches and retired branches.

Instructions retired are deterministic for a fixed input. They do not depend
on the core a thread runs on, on its frequency, or on branch predictor state,
so they give nanosecond-scale operations a metric with real resolution, which
wall time does not have. Cycles are wall time without frequency scaling.
Branch misses explain most cycle-only differences. Counting is per thread and
survives migration between cores.

```rust
let counters = tessera_pmu::Counters::open()?;
let reading = counters.measure(100_000, || work());
println!("{} instructions per call", reading.instructions / 100_000);
```

## Platform and privileges

Only macOS on Apple silicon is implemented, through the private `kperf` and
`kperfdata` frameworks loaded with `dlopen`; Linux is not planned. The
benchmarks are run locally, not in CI.

Programming the counters (`kpc_force_all_ctrs_set`, `kpc_set_config`,
`kpc_set_counting`, `kpc_set_thread_counting`) needs root; the first of them
is the one that fails without it. `Counters::open` fails with the reason
(missing framework or event, no privileges), and there is no fallback to
timing: a measurement without counters is an error, not a degraded result.

```sh
sudo -v
cargo build --release -p tessera-pmu --example count --locked
sudo -n target/release/examples/count
```

The example sums 1024 `u32` values 100 000 times and prints the counts per
call. On an M5 Pro it prints 796 instructions, about 200 cycles, 69 branches
and 0 branch misses per call; the instructions repeat exactly between runs,
the cycles move by a fraction of a percent. This is the check that the
setup works.

## What is counted

Event names come from the `kpep` database of the running CPU
(`/usr/share/kpep/<chip>.plist`; the running chip is selected by passing no
name to `kpep_db_create`): `INST_ALL`, `CORE_ACTIVE_CYCLE`,
`BRANCH_MISPRED_NONSPEC` and `INST_BRANCH`. Every event is added with the
`kpep_config_add_event` flag that restricts it to user space (EL0), so the
kernel's own work in interrupts and context switches is not counted. The
fixed instruction and cycle counters are not used because they cannot
exclude the kernel: they leaked a few thousand instructions into some
blocks. Counting is per thread and survives migration between cores.

`cpu_number` returns the CPU the calling thread is on. It is not part of a
measurement, but it explains cycle modes: each core keeps its own predictor
state.

## Known failures

- `kpc_force_all_ctrs_set failed with code ...`: not root. Cache the
  credentials with `sudo -v` and start the process with `sudo -n`.
- `PMU event ... is not in this CPU's kpep database`: a new chip whose
  database names the events differently. Look up the names in the chip's
  plist under `/usr/share/kpep` and adjust `EVENTS` in `src/macos.rs`.
- A missing symbol or framework: the frameworks are private API and can
  change between macOS versions. `src/macos.rs` lists every symbol used.
- Instruments and `xctrace` take the counters away while they record; a
  measurement started meanwhile fails to open or reads zeros.
- A block occasionally reads zero instructions or cycles although the code
  ran. The cause is not known; the benchmark runner detects it and repeats
  the block, and tessera-bench reports an operation as `UNSTABLE` if zeros
  remain.

This crate is tooling and is never linked into the library. It is the one
place outside `tessera-capi` that uses `unsafe`; every block states its
invariants.
