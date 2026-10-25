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

The crate counts in four cells, with the same four events in each:

```text
             AArch64                      x86-64
  macOS      kperf, Apple silicon         kperf, Intel
  Linux      perf_event_open, Arm PMU     perf_event_open, Intel or AMD
```

On every other system or processor, and under Rosetta,
[`Counters::open`] fails with [`ErrorKind::Unsupported`]. Numbers of
two cells do not compare: the instructions and the machines differ.
What compares is a change against its base inside one cell.

### macOS

The crate loads the private frameworks `kperf` and `kperfdata` with
`dlopen`, so that nothing links against them at build time.
Programming the counters needs root: `kpc_force_all_ctrs_set`,
`kpc_set_config`, `kpc_set_counting` and `kpc_set_thread_counting` all
refuse another user, and the first of them is the one that fails.
Build the program as yourself, run `sudo -v` once, and then only the
program with `sudo -n`, which fails instead of asking for the password
again:

```sh
cargo build --release -p pmu --example count --locked
sudo -v
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
of the same meaning. Each is added with the flag that restricts it to
user space. The fixed counters of instructions and cycles are not used,
because they count the kernel too: they added a few thousand
instructions to some blocks.

kperf programs the counters of the whole system. The first `open` that
succeeds sets them up for the process, every later one reuses that
setup, and the process holds the counters until it exits.

### Linux

The crate opens the four events as one group of the calling thread with
`perf_event_open`, on the PMU of the processor's cores that sysfs lists
under `/sys/bus/event_source/devices`. One `read` gives all four counts
at one instant, with the times the group was enabled and ran; a group
that lost the PMU for a while gives an error, never a scaled count.

- On x86-64 the events are the kernel's generic ones: instructions,
  cycles, branch instructions and branch misses, which the kernel maps
  to the retired events of Intel and of AMD.
- On AArch64 they are the Arm architecture's `INST_RETIRED`,
  `CPU_CYCLES`, `BR_RETIRED` and `BR_MIS_PRED_RETIRED`, the events of
  macOS on the M4 and later. The kernel's generic branch events count
  something else on Arm. A core without the two branch events, such as
  the Cortex-A72 of a Raspberry Pi 4, is refused, and so is a PMU of
  another kind, such as that of Apple's cores under Asahi Linux.

No root is needed where `kernel.perf_event_paranoid` lets a user count
its own threads, as at 2, the kernel's default. Debian refuses at 3 and
Ubuntu at 4, their defaults; there root lowers the setting, or gives the
program `CAP_PERFMON` where the kernel accepts it:

```sh
sudo sysctl kernel.perf_event_paranoid=2
```

A processor with two kinds of cores, such as Intel's since the 12th
generation or an Arm board with big and little cores, has a PMU for
each kind, and an event of one kind does not count while the thread runs
on the other. The crate counts only a thread whose CPUs all belong to
one kind, and its error names the CPUs of each kind. Bind the program:

```sh
taskset -c 0-7 target/release/examples/count
```

A virtual machine counts only when its hypervisor gives it a PMU; most
clouds give none to small machines, and no hypervisor of a Mac does.

## Errors

[`Counters::open`] and the reads fail with an [`Error`]. Its
[`kind`](Error::kind) names the cause for code, and its message names
the cause and the fix for people; [`ErrorKind`] lists the kinds and
their fixes. The common ones:

- [`ErrorKind::Permission`]: on macOS, not root; on Linux,
  `kernel.perf_event_paranoid` refuses the user.
- [`ErrorKind::NoCounters`]: a virtual machine without a PMU, or a
  virtual Mac, whose kernel has no kpc.
- [`ErrorKind::Busy`]: on Linux, a group that does not stay on the
  PMU, as when the NMI watchdog holds a counter; on macOS, kperf
  refuses the counters to root.
- [`ErrorKind::MixedCores`]: on Linux, a thread that may run on two
  kinds of cores.
- [`ErrorKind::MissingEvent`]: a core without an event the crate
  counts, or a new Apple chip whose database names an event otherwise.
- [`ErrorKind::Unsupported`]: another system or processor, Rosetta, or
  a version of macOS whose private frameworks changed.
- [`ErrorKind::Lost`]: a read after the counters stopped counting.

## Known failures

A block now and then reads zero instructions or cycles on Apple
silicon although its code ran. The cause is not known. A caller should
take a zero as a failed block and repeat it.

Two programs that count at once on macOS spoil each other's counts.
The kernel refuses no second process: its setup reprograms the
counters under the first, whose reads then fail with
[`ErrorKind::Lost`] or give wrong counts without an error. Count with
one program at a time, and not while Instruments or `xctrace` records
counters.

## License

BSD-2-Clause, in the file `LICENSE` beside this one.
