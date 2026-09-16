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

Only macOS is implemented, through the private `kperf` and `kperfdata`
frameworks loaded with `dlopen`. Programming the counters needs root:

```sh
sudo -v
sudo -n target/release/examples/count
```

`Counters::open` explains the failure otherwise (missing framework or event,
no privileges). There is no fallback to timing: a measurement without
counters is an error, not a degraded result.

The frameworks are private API. They can change between macOS versions, and
Instruments takes the counters away while it records. Event names come from
the `kpep` database of the running CPU (`/usr/share/kpep`); the four used
here exist on every Apple Silicon generation so far.

This crate is tooling and is never linked into the library. It is the one
place outside `tessera-capi` that uses `unsafe`; every block states its
invariants.
