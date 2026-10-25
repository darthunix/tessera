# pmu

Four hardware counters of one thread, in user space: retired
instructions, core cycles, retired branches and mispredicted retired
branches. They are for benchmarks of code that runs for nanoseconds,
where wall time has no resolution.

```rust,no_run
use pmu::Counters;

let counters = Counters::open()?;
let values: Vec<u32> = (0..1024).collect();
let calls = 100_000;
let reading = counters.measure(calls, || {
    values.iter().map(|&v| u64::from(v)).sum::<u64>()
})?;
println!("{} instructions per call", reading.instructions / calls);
# Ok::<(), pmu::Error>(())
```

## What is counted

The count of retired instructions is the same for the same input. It
does not depend on the core a thread runs on, on the core's frequency,
or on the state of the branch predictor, so it gives code that runs for
nanoseconds a measure with real resolution. Cycles are the core's clock
periods while the thread ran in user space. The same work costs the
same cycles at any frequency as long as it stays in the core and its
first cache; the latency of memory is fixed in nanoseconds, not in
cycles. Time off the core and in the kernel is not counted. Branch
misses explain most differences seen only in cycles.

Counting is per thread: [`Counters`] counts the thread that opened it,
on whichever core it runs. Each counter counts user space only, so the
kernel's work for the thread, in system calls, interrupts and page
faults, is left out. A reading is exact or the call fails; nothing is
scaled or estimated, and there is no fallback to time.

[`Counters::measure`] reads the counters around a loop of calls of a
closure and gives the difference. [`Counters::read`] gives the counts
since [`Counters::open`], so a caller can bracket any code itself, and
[`Reading::since`] subtracts two readings. [`cpu_number`] tells the
CPU the calling thread runs on: it is not part of a measurement, but
it explains why the cycles of one block differ from another's, since
each core keeps its own predictor state.

## Platforms and privileges

For now the crate counts only on macOS, through the private frameworks
`kperf` and `kperfdata`, which it loads with `dlopen` so that nothing
links against them at build time. On every other system
[`Counters::open`] fails.

Programming the counters needs root: `kpc_force_all_ctrs_set`,
`kpc_set_config`, `kpc_set_counting` and `kpc_set_thread_counting` all
refuse another user, and the first of them is the one that fails. Run
`sudo -v` once, and then the measuring program with `sudo -n`, which
fails instead of asking for the password again:

```sh
sudo -v
cargo build --release -p pmu --example count --locked
sudo -n target/release/examples/count
```

The example sums 1024 `u32` values 100 000 times and prints the counts
per call. On an M5 Pro it prints 796 instructions, about 200 cycles, 69
branches and 0 branch misses per call. The instructions repeat exactly
between runs; the cycles move by a fraction of a percent. This is the
check that the setup works.

The event names come from the `kpep` database of the running processor,
`/usr/share/kpep/<chip>.plist`: `INST_ALL`, `CORE_ACTIVE_CYCLE`,
`INST_BRANCH` and `BRANCH_MISPRED_NONSPEC`. From the M4 on they are the
Arm architecture's events `INST_RETIRED`, `CPU_CYCLES`, `BR_RETIRED` and
`BR_MIS_PRED_RETIRED`; on the M1 to the M3 they are Apple's own events
of the same meaning. Each is added with the flag that restricts
it to user space. The fixed counters of instructions and cycles are not
used, because they count the kernel too: they added a few thousand
instructions to some blocks.

## Errors

[`Counters::open`] and the reads fail with an [`Error`]. Its
[`kind`](Error::kind) names the cause for code, and its message names
the cause and the fix for people; [`ErrorKind`] lists the kinds and
their fixes. On macOS:

- not root: [`ErrorKind::Permission`]. Cache the credentials with
  `sudo -v` and start the program with `sudo -n`.
- another task holds the counters, such as Instruments, `xctrace` or
  another program that counts: [`ErrorKind::Busy`].
- a virtual Mac, whose kernel has no kpc: [`ErrorKind::NoCounters`].
- an x86-64 program under Rosetta, or a version of macOS whose private
  frameworks changed: [`ErrorKind::Unsupported`].
- a new chip whose database names an event otherwise:
  [`ErrorKind::MissingEvent`]. Its plist under `/usr/share/kpep` lists
  the names.

## Known failures

A block now and then reads zero instructions or cycles on Apple
silicon although its code ran. The cause is not known. A caller should
take a zero as a failed block and repeat it.

## License

BSD-2-Clause, in the file `LICENSE` beside this one.
