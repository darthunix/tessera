# pmu: how it is built

A kernel of Tessera is judged by what it costs the processor: how many
instructions it retires, how many cycles it takes, how many branches it
runs and mispredicts. Times on a busy machine move by percents; these
counts repeat. The crate `pmu` gives them: four counts of one thread,
in user space, from the processor's performance counters. Tessera's
benchmark programs measure blocks of calls with it, and people outside
Tessera can take it as it is.

This page explains how the crate reaches the counters on each system,
why the four counts mean the same thing everywhere, what it does when
they cannot be had, and how each part is checked. The rules of the
API, its errors and its limits are written once, in the crate's
documentation, which the spec, [spec.md](spec.md) next to this page,
links to. The rules of the crate's build are its contract,
[contract.md](contract.md).

## The problem

A processor counts events in a few registers of its own, its PMU
(performance monitoring unit). A program cannot program them itself:
the system does it, and each system has its own way.

- macOS gives the private frameworks kperf and kperfdata. kperfdata
  holds a database of each processor's events, kpep, and turns names
  of events into the words that program the counters. kperf, through
  the kernel's kpc, programs them and reads a thread's counts. It needs
  root.
- Linux gives the system call `perf_event_open`. An event opens as a
  file descriptor, and a read of it gives its count. A user may count
  its own threads where the setting `kernel.perf_event_paranoid`
  allows it.

The vector code of Tessera comes in two sets: NEON on AArch64 and AVX2
on x86-64. A developer may write a kernel on any of four pairs of a
system and a processor, the four cells:

```
            NEON (AArch64)              AVX2 (x86-64)
  kperf     macOS, Apple silicon        macOS, Intel
  perf      Linux, AArch64              Linux, x86-64
```

A count is worth something only if it means the same thing in every
cell: the same event, of the same thread, in the same mode. A count of
the kernel's work, of another thread, or a count scaled from a part of
the time, would look like a count and judge a kernel wrongly.

## Goals and what they cost

- **The same four counts in every cell.** User space only, of the
  thread that opened the counters, events that retire. The cost: each
  backend picks its events by their meaning, not by their name, and a
  processor that lacks one is refused rather than counted with another.
- **Exact or an error.** A reading is never scaled, estimated or made
  from time. The cost: where the counters cannot be had, there is no
  reading at all, and the caller sees why.
- **One API on every target.** A caller writes no `cfg` of its own.
  The cost: on a target without a backend, `open` exists and fails.
- **An error that says what to do.** Each failure names its cause and
  its fix: run as root, lower a setting, bind the thread, count on
  hardware. The cost: the backends tell causes apart by the codes of
  the system, which differ between its calls.
- **A crate that stands alone.** It builds, passes its tests and packs
  without any crate of Tessera, and CI watches its public API. The
  cost: its own version, license and page.
- **A read that costs the same each time.** One call into the kernel
  and nothing allocated, so that its cost inside a measured block is
  small and fixed. The cost: no extra checks on the path of a read.

## The whole in one picture

```
                        pmu::Counters::open / read
                                   |
             macOS (kperf)         |        Linux (perf_event_open)
       +---------------------------+---------------------------+
       |                                                       |
  kpep database of the CPU                     the core PMU, chosen
  four names, flag "user only"                 from sysfs
    AArch64: Apple silicon                       x86-64: generic ids
    x86-64:  Intel                               AArch64: Arm events
  set up once for the process                  one pinned group of
  needs root                                   the calling thread
                                               no root if the setting
                                               allows the user

  any other target: open fails with ErrorKind::Unsupported
```

`Counters::open` sets the counters up and starts them for the calling
thread. `Counters::read` gives the four counts since then, as a
`Reading`. `Counters::measure` reads around a loop of calls of a
closure and gives the difference. `cpu_number` tells which CPU the
thread runs on now. Each backend is a module chosen by `cfg`; the
public items are the same everywhere.

## The words used below

- **PMU**: the counters of a processor and the registers that program
  them.
- **Core PMU**: the PMU of the processor's cores, as opposed to the
  counters of the rest of the chip, such as its memory controller.
- **Cell**: a pair of a system's way to the counters, kperf or perf,
  and a set of vector instructions, NEON or AVX2.
- **User space**: the code of the program itself, without the work the
  kernel does for it in system calls, interrupts and page faults.
- **Pinned group**: on Linux, events opened together, read at one
  instant, that must stay on the PMU all the time or count nothing.

## One API on every target

`open`, `read` and `measure` return `Result`. `Error` holds a kind,
`ErrorKind`, a message that names the cause and the fix, and the
system's error as its source where there is one. `ErrorKind` is
`#[non_exhaustive]`, so a new cause is not a breaking change; the
kinds and their fixes are listed in its documentation. `cpu_number`
returns `None` where the system does not say.

`Reading` has four public fields, `+`, `+=` and `since`, and
serializes with serde behind the feature `serde`. It stays exhaustive,
so that callers build it with a literal; a fifth count will be a new
major version.

`read` returns an error because a read can fail for a reason of the
machine, not of the caller. On Linux a pinned group that loses the PMU,
to another pinned user or to the NMI watchdog, reads nothing. On macOS
a read fails when its system call fails, as when another process has
set kperf up since `open` (see "kperf on macOS"). A library does not
panic on such a state, and a `Result` costs a branch.

Refused:

- `anyhow::Error`. A caller cannot match on its cause.
- A `read` that panics. It ends a whole benchmark for one lost block.
- An API behind `cfg`, present only where a backend is. Every caller
  would repeat the `cfg`.
- A `#[non_exhaustive]` `Reading`. It forbids the literal and the
  struct update outside the crate.

## One thread

`Counters` counts the thread that opened it and is read on that
thread. It holds a marker that is neither `Send` nor `Sync`, so the
compiler refuses to move it or share it. On Linux an event's file
descriptor counts the thread that opened it, and a read on another
thread would give the first thread's counts without a word. A program
that measures several threads opens counters in each.

Refused:

- A group for each thread, opened on that thread's first read. An open
  could then fail in the middle of a measurement.
- A check of the thread on every read. It costs a system call on each
  read, and fails at run time where the compiler can refuse.

## kperf on macOS

The backend loads kperf and kperfdata with `dlopen`, so that nothing
links against private frameworks at build time. A missing framework or
symbol fails with `ErrorKind::Unsupported`, naming it: a later macOS
may change them.

**The events.** `open` asks kpep for the database of the running
processor and adds four names to a configuration, each with the flag
that counts user space only: `INST_ALL`, `CORE_ACTIVE_CYCLE`,
`INST_BRANCH` and `BRANCH_MISPRED_NONSPEC`. The database turns them
into the configuration words of programmable counters.

- From the M4 on, the names are the Arm architecture's events
  `INST_RETIRED`, `CPU_CYCLES`, `BR_RETIRED` and
  `BR_MIS_PRED_RETIRED`: the events the Linux backend programs on
  AArch64, so both NEON cells count the same events. On the M1 to the
  M3 they are Apple's own events of the same meaning.
- On an Intel Core i5-1038NG7 (Ice Lake), the names stand for
  `INST_RETIRED.ANY`, `CPU_CLK_UNHALTED.THREAD`,
  `BR_INST_RETIRED.ALL_BRANCHES` and `BR_MISP_RETIRED.ALL_BRANCHES`,
  the events Linux maps its generic ids to on Intel. The database
  gives the words of programmable counters, `0x4100c0`, `0x41003c`,
  `0x4100c4` and `0x4100c5`, each with the bit that counts user mode
  only. One list of names serves both kinds of Mac.
- The fixed counters of instructions and cycles are not used: on Apple
  silicon they count every mode, and timer interrupts inside a block
  added thousands of kernel instructions to it.

The test that the kernel's work is not counted shows the flag holds:
a call of `getppid` counts 12 instructions beyond an empty loop, on an
M5 Pro and on the Ice Lake alike.

**Root and errors.** Programming the counters needs root. The `kpc_*`
calls return -1 when they fail and leave the cause in `errno`, which
the error keeps as its source: `EPERM` from a process that is not root
is `ErrorKind::Permission`; `EACCES` or `EBUSY` from a root process is
`ErrorKind::Busy`. The `kpep_*` calls return codes of their own, which
the message names.

**Rosetta and virtual Macs.** An x86-64 program under Rosetta reads an
Intel processor that kpep has no database for, and its counts would be
those of the translated code. `open` asks `sysctl.proc_translated`
before anything else, which needs no root, and fails with
`ErrorKind::Unsupported`, naming Rosetta. A virtual Mac has no kpc in
its kernel, and kpep has no database for its virtual processor: when
the database is missing and `kern.hv_vmm_present` says a virtual
machine, `open` fails with `ErrorKind::NoCounters`.

**Once for the process.** kperf programs the counters of the whole
system, not of a thread. A second setup while another thread of the
process counts would reprogram them under that thread's block. So the
first `open` that succeeds sets kperf up for the process, and every
later `open`, on any thread, counts with that setup; a setup that fails
is not kept, so a later `open` tries again. The process holds the
counters until it exits.

**A second process.** The kernel does not keep a second process off
the counters. In XNU's source kpc is a layer over the kernel's CPC:
`cpc_task_set_owner` finds that another task holds the counters and
still returns success, and `kpc_set_config` reprograms them in any
case. So a second process that sets kperf up spoils the counts of the
first. As root on an Intel Mac, a read of the first then failed with
`EINVAL`, which is `ErrorKind::Lost`, and of two equal loops one
counted 1.45 times the instructions of the other. The documentation of
`Counters` and the README say to count with one program at a time, and
not beside Instruments or `xctrace`.

**Zero blocks.** On Apple silicon a block now and then reads zero
instructions or cycles although its code ran. Such reads are rare,
their cause is not known, and the zero is their only sign. The
documentation of `measure` and the README say to take a zero as a
failed block and repeat it, as Tessera's benchmark runner does.

Refused:

- A list of Intel names beside the Apple ones. The database gives the
  same configuration words for the one list.
- A release of the counters when the last `Counters` is dropped. It
  would need a count of the open ones across threads, and a release
  under a thread that opens again would race with it.
- A lock file that `open` holds for the life of the process, so that a
  second program on `pmu` fails with `ErrorKind::Busy`. It would keep
  off only programs on `pmu`, not Instruments or `xctrace`, and the
  person who starts the programs knows what runs.
- A `measure` that fails with `ErrorKind::Lost` on a zero. It would
  judge only `measure`, not the difference of two reads a caller takes
  itself, so the documentation would still have to warn of zeros; and
  every caller would handle as an error a block it repeats now.
- Counting on macOS without root. The maintainer left it out.

## perf_event_open on Linux

`open` opens four events on the core PMU (see the next section) as one
group of the calling thread; the instruction counter leads it.

- One `read(2)` of the leader gives all four, taken at one instant,
  with the time the group was enabled and the time it ran.
- `pid = 0, cpu = -1`, without inheritance: the calling thread, on any
  CPU, without its children.
- `exclude_kernel` and `exclude_hv`: user space only, as on macOS.
  Cycles are the unhalted cycles of the core while the thread ran in
  user mode, the meaning `CORE_ACTIVE_CYCLE` has on macOS.
- The leader is pinned. A pinned group that cannot stay on the PMU goes
  into an error state, and a read of it gives end of file; it is never
  multiplexed. A read that is short, holds a count of events other than
  four, or whose two times differ, fails with `ErrorKind::Lost`.
- The leader starts disabled. Once the group is complete, one `ioctl`
  resets and one enables the whole group. Then `open` reads it once:
  a group that cannot stay on the PMU fails there, with
  `ErrorKind::Busy` and the name of `kernel.nmi_watchdog`, rather than
  at the first measurement.
- The structure `perf_event_attr`, the number of the system call and
  the codes of the `ioctl`s come from `perf-event-open-sys`; `read`,
  `close` and `sched_getcpu` come from `libc`.
- Each error of `open` maps to its kind. `ENOENT`, `EOPNOTSUPP`,
  `ENODEV` or `ENOSYS`: no PMU, or no `perf_event_open`, as in a
  virtual machine without a virtual PMU or under QEMU's emulation of
  another processor. `EACCES` or `EPERM`: a setting of
  `kernel.perf_event_paranoid` that refuses the user; the message gives
  its value. `EBUSY`: another program holds the PMU for itself.

No root is needed while `kernel.perf_event_paranoid` is 2 or less, the
kernel's default: any user may count its own threads in user space.
Debian refuses at 3, its default. Ubuntu refuses at 4, its default,
and at 3 still lets a user count in user space. Where the setting
refuses, root lowers it to 2, or gives the program `CAP_PERFMON` where
the kernel accepts it.

Refused:

- Reading the counters with `rdpmc`, through the page that
  `perf_event_open` can map. A read is two per block of calls, and a
  `read(2)` is one system call, as `kpc_get_thread_counters` is on
  macOS. `rdpmc` adds a retry loop, assembly and unsafe code, and on
  Arm it needs a setting only root can change. It is worth it only if
  a measurement shows that the cost of a read matters.
- Scaling a multiplexed count by its times. A scaled count is an
  estimate.
- The ABI declared by hand. `perf-event-open-sys` is generated from the
  kernel's headers for every architecture and depends on `libc` alone.
- The crate `perf-event`. It has no raw events, which AArch64 needs.

## The core PMU and its four events

`open` finds the core PMU in `/sys/bus/event_source/devices`, as the
`perf` tool does: the device `cpu`, or every device with a file `cpus`
that lists its CPUs. The devices of the rest of the chip have no such
file.

- **x86-64**: the type `PERF_TYPE_HARDWARE` with the generic ids of
  instructions, CPU cycles, branch instructions and branch misses. The
  kernel maps them to retired events, on Intel and on AMD alike. On a
  processor with two kinds of cores, the type of the chosen PMU goes
  into the high 32 bits of each id, as the kernel asks.
- **AArch64**: raw events of the Arm PMU architecture on the type of
  the chosen PMU: `INST_RETIRED` (0x08), `CPU_CYCLES` (0x11),
  `BR_RETIRED` (0x21) and `BR_MIS_PRED_RETIRED` (0x22). The kernel's
  generic ids are not used: it maps branch misses to `BR_MIS_PRED`,
  which counts speculative mispredictions, and branches to an event
  whose meaning has changed between kernels. The driver lists a PMU's
  events as files under `events/`, and opens a raw event the core
  lacks, or that a hypervisor hides from its guest, which then counts
  nothing. So `open` checks all four before it opens anything, and
  names the one missing, `ErrorKind::MissingEvent`; some Arm cores lack
  the branch events, such as the Cortex-A72. Only the Arm
  architecture's PMUs, `armv8_` and `armv9_`, are taken: another kind,
  such as that of Apple's cores under Asahi Linux, is refused with
  `ErrorKind::Unsupported`.
- The choice takes the root of the sysfs tree, the thread's CPUs, the
  online CPUs and the architecture as arguments; only `open` fills them
  from the running system. Its tests then run on any host, over trees
  of files written for the test.

GitHub's Arm runner, a Neoverse N2 guest, has one core PMU,
`armv8_pmuv3_0`, with six programmable counters and the cycle counter,
and no NMI watchdog; its `events/` lists all four events, and its
`kernel.perf_event_paranoid` is 4. Its x86-64 runners have no PMU.

Refused: raw codes of Intel events. The kernel already maps the
generic ids, and the same ids then count on AMD.

## Two kinds of cores

Some processors have two kinds of cores: Intel's since the 12th
generation (`cpu_core` and `cpu_atom`), Arm boards with big and little
cores, Apple silicon. The instructions and the branches of a loop do
not depend on the kind; the cycles and the branch misses do.

On Linux each kind has its own core PMU, and an event bound to one
kind does not count while its thread runs on the other, without a
word. So when more than one core PMU is listed, `open` reads the CPUs
the thread may run on, `Cpus_allowed_list` of
`/proc/thread-self/status`, without the offline ones. If they all
belong to one core PMU, the group opens on it. If not, `open` fails
with `ErrorKind::MixedCores`, naming each kind with its CPUs and a
command that binds the process to one, `taskset -c <cpus> <command>`.
A single core PMU that covers only some of the CPUs is taken the same
way: the thread must be bound to them.

macOS has no call that binds a thread to one kind of core, and kperf
programs the same events on every core. So a thread is counted on
whichever kind it runs, and a reading sums both. The documentation of
`Reading` says so, and a caller that needs to know where a block ran
asks `cpu_number` at both ends of it, as Tessera's benchmark programs
do.

Refused:

- A group for each kind of core on Linux, the readings added up, as
  macOS does by itself. It would give Linux the meaning of the macOS
  cells, at the cost of two groups read at two instants. The
  maintainer chose the binding, which keeps cycles in one unit where
  the system allows it.
- Binding the thread from inside `open`. A library that changes the
  affinity of its caller surprises it; the caller binds.

## The package

The crate lies in `tools/pmu`, among the tools that measure Tessera.

- `publish = true`, which makes its public API a surface of the spec.
- Its own version, which follows its API, not Tessera's releases.
- The license BSD-2-Clause, with a `LICENSE` file beside the manifest,
  since the license asks that its notice travel with the code. It is
  permissive like PostgreSQL's, so the crate lives in Tessera's
  repository, and the license checks of Rust projects know it.
- The least Rust version is the workspace's. CI builds the crate with
  exactly that toolchain, so the promise is checked without a second
  one. When Tessera raises its toolchain, the crate's least version
  rises with it, in a new minor version.
- The README is the crate's page, and every public item is documented;
  a missing comment fails the build.
- Dependencies: on Linux for x86-64 and AArch64, `perf-event-open-sys`
  and `libc`; `serde` behind the feature `serde`. Nothing else.

Refused:

- `publish = false`. Its API would not be a surface of a spec, and
  nothing would keep it fit to publish.
- `crates/pmu`. `crates/` holds the parts of Tessera itself, and the
  counters are a tool that measures them. Where a crate lies does not
  decide whether it is published.
- The license PostgreSQL, the workspace's. It allows the same, but the
  license checks of Rust users rarely list it.
- `MIT OR Apache-2.0`, the habit of Rust crates. Apache-2.0 adds terms
  that PostgreSQL's license does not have; BSD-2-Clause is closer.
- A lower Rust version than Tessera's. Keeping that promise needs a
  second toolchain in CI; it can come when a caller asks for it.

## How it is checked

**Tests that say whether they may skip.** A test that cannot open the
hardware counters prints why and passes, so that a run without them
stays green and still shows the cause. `PMU_REQUIRE` turns the skip
into a failure:

- `software`: on Linux, a test of the plumbing must open a group of
  four software events, which runs the same code as the hardware group
  but for the type and the id of each event.
- `hardware`: every test must also open the hardware counters.
- Any other value fails every test, so that a misspelt name does not
  skip.

`PMU_EXPECT_ERROR` names the kind of error `open` must fail with, on a
machine whose answer the run knows.

**The kernel's work.** A test calls a cheap system call, `getppid`,
many times. In user space a call costs a few instructions; in the
kernel, hundreds. The loop of the calls is measured alone and taken
away, since in a debug build it costs about a hundred instructions
itself. Beyond it a call counts 12 instructions on GitHub's Arm
runner, on an M5 Pro and on an Intel Core i5-1038NG7, and about 800
with the kernel on the Arm runner; the test fails above 100.

**Two kinds of cores on a Mac.** A test measures the same loop on a
thread of the user-interactive class and on one of the background
class, which macOS tends to run on different kinds of cores. Their
instructions must be equal within the cost of the reads, and the test
logs `cpu_number` at both ends of each. No call of macOS puts a thread
on a chosen kind, so the test shows that counting goes on wherever the
thread runs, not which kind it ran on.

**Each cell where it can be.**

- perf on AArch64: GitHub's Arm runner, at every push. The tests run
  first at the image's `kernel.perf_event_paranoid`, 4, and must fail
  with `ErrorKind::Permission`; then the job lowers it to 2 and runs
  them with `PMU_REQUIRE=hardware`.
- perf on x86-64: GitHub's x86-64 runner, which has no PMU. `open`
  must fail with `ErrorKind::NoCounters`, the software group runs with
  `PMU_REQUIRE=software`, and the choice of events is tested over
  written trees. No machine with a PMU checks this cell.
- kperf on both kinds of Mac: as root, by hand, since a Mac in CI is a
  virtual machine without kpc. The tests are built as the user, and
  only their binaries run as root, through Cargo's runner of the
  target after `sudo -v`, such as
  `CARGO_TARGET_AARCH64_APPLE_DARWIN_RUNNER='sudo -n env PMU_REQUIRE=hardware'`.
  So `target/` gets no files owned by root, and a binary run after
  sudo's time is out fails instead of asking for the password again.
  The example counts as the README says.
- On the Mac of CI, `open` must fail with `ErrorKind::NoCounters`, and
  a build for x86-64 under Rosetta with `ErrorKind::Unsupported`.

The examples of the documentation that open the counters are built but
not run (`no_run`): a doctest cannot skip, and as root rustdoc's
doctests run as processes at once and would spoil each other's counts.

**The public API.** CI lists it with `cargo public-api` on the host and
on four more targets, FreeBSD among them, which has no backend, and
compares each list with one snapshot, `public-api.txt` beside this
page. CI also runs Clippy for the four targets of the cells and for
FreeBSD, and a dry run of publishing.

## What we decided not to do

- No fallback to time or to software events when the hardware counters
  cannot open.
- No multiplexed or scaled counts.
- No `rdpmc` until a measurement asks for it.
- No sum over two kinds of cores on Linux, and no binding from inside
  `open`.
- No counting of other threads or of child processes.
- No Linux under Asahi, and no counting under Rosetta.
- No counting on macOS without root.
- No guard against a second process on macOS.

## Files

- `tools/pmu/Cargo.toml`: the package, its feature and its
  dependencies by target
- `tools/pmu/README.md`: the crate's page: what is counted, the cells
  and their privileges, the errors, the known failures
- `tools/pmu/src/lib.rs`: the counters, the reading and the number of
  the CPU, and the choice of the backend
- `tools/pmu/src/error.rs`: the error and its kinds, with the fix of
  each kind
- `tools/pmu/src/macos.rs`: the kperf backend
- `tools/pmu/src/linux.rs`: the perf backend: the group, its read and
  its errors
- `tools/pmu/src/core_pmu.rs`: the choice of the core PMU and of its
  four events, over sysfs
- `tools/pmu/src/unsupported.rs`: the backend of every other target
- `tools/pmu/examples/count.rs`: the example that checks a setup
- `openspec/specs/pmu/public-api.txt`: the snapshot of the public API

## Tests

- The unit tests in `tools/pmu/src/core_pmu.rs`: the lists of CPUs, and
  the choice over written trees: one x86 PMU, two kinds of x86 cores,
  big and little Arm cores, one PMU over some CPUs, offline CPUs, a
  missing Arm event, a PMU of another kind, no core PMU.
- The unit tests in `tools/pmu/src/linux.rs`, on Linux: the software
  group through the code of the hardware group, the reads of a lost
  group, the kernel's generic ids, and the CPU of the thread.
- The unit tests in `tools/pmu/src/lib.rs` and
  `tools/pmu/src/error.rs`: the difference of two readings, a reading
  as data, and the parts of an error; two doctests that must not
  compile, `Counters` moved to or shared with another thread.
- `tools/pmu/tests/open.rs`, on the machine at hand: a known loop, the
  kernel's work, the error a run expects, a second thread that opens,
  and on a Mac the two kinds of cores.
- The job "pmu alone" of `.github/workflows/ci.yml` runs them on
  GitHub's x86-64, Arm and macOS runners, as "Each cell where it can
  be" says.
