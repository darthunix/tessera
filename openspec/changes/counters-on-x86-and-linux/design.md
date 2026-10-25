## Context

`tools/tessera-pmu` is a small crate of the workspace, not published
and never linked into Tessera. It gives one type, `Counters`, with two
calls: `open` sets the counters up, `read` returns four numbers of the
calling thread. `measure` reads twice around a loop of calls. The one
backend is kperf, loaded with `dlopen`; it needs root.

The benchmark programs of `crates/tessera-capi/benches` open the
counters once, on their main thread, and measure blocks of calls.
`tessera-bench` starts them and judges the readings; it does not link
the crate.

This change makes the crate one that people outside Tessera can take:
the four counts in the four cells of the proposal, with the same
meaning in each, behind one API, with errors that say what to do. The
proposal gives the findings. In short: the kperf backend likely works
on Intel already, no virtual machine on a Mac counts, and GitHub's Arm
runner does.

```
                        pmu::Counters::open / read
                                   |
             macOS (kperf)         |        Linux (perf_event_open)
       +---------------------------+---------------------------+
       |                                                       |
  kpep database of the CPU                     the core PMU, chosen
  four names, flag "user only"                 from sysfs
    AArch64: Apple silicon                       x86-64: generic ids
    x86-64:  Intel, to be checked                AArch64: Arm events
  needs root                                   one pinned group of
                                               the calling thread
                                               no root if the setting
                                               allows the user

  any other target: open fails with ErrorKind::Unsupported
```

## Goals / Non-Goals

**Goals:**

- The same four counts, with the same meaning, in the four cells:
  user space only, of one thread, exact or an error.
- One API on every target, documented as docs.rs shows it, so that a
  caller writes no `cfg` of its own.
- An error that names its cause and its fix.
- A crate ready to publish: it builds, passes its tests and packs
  alone, and CI watches its public API.
- Each cell checked where it can be: in CI where a runner counts, as
  root on the maintainer's Macs otherwise.

**Non-Goals:**

- Publishing on crates.io; the maintainer decides it after the change.
- Any change of `tessera-bench`, or of Tessera's documents beyond a
  link. They come in the next stage.
- Comparing numbers across cells. NEON and AVX2 are different sets of
  instructions, and the machines differ. What compares is a verdict
  inside one cell: a change against its base, a vector path against
  the row path it replaces.
- Counting without a PMU: no fallback to software events or to time.
- Counting on macOS without root.
- A check of x86-64 Linux on hardware: no such machine is at hand.

## Decisions of the maintainer

- 2026-10-04: x86 and Linux are planned. A run will go differently
  there than on macOS, where kperf needs root.
- 2026-10-05: this work comes before the AVX2 kernels, so that the
  tool to measure them is there while they are written.
- 2026-10-24: the goals are AVX2 kernels beside NEON, a tool for
  developers on Linux, and counting in the four cells. The first stage
  changes only the crate. A processor with two kinds of cores is
  counted when the process is bound to one kind.
- 2026-10-25: the crate is a crate of its own that may be published.
  No container in the repository. Its name is `pmu`. The Linux ABI
  comes from `perf-event-open-sys`. CI checks perf on AArch64 with real
  counts. The license is compatible with PostgreSQL's, so that the
  crate lives in Tessera's repository. The least Rust version is
  Tessera's, so that Tessera's toolchain does not change for the
  crate. No counting on macOS without root.

## Decisions

### A crate of its own

Was: `tools/tessera-pmu`, `publish = false`, the workspace's version
and license, `anyhow` and `serde` as dependencies.

Will be: `crates/pmu`, the package and library `pmu`.

- `publish = true`, which makes its public API a surface of a spec. It
  is not published by this change.
- Version 0.1.0, its own. A published crate's version follows its API,
  not Tessera's releases.
- The license BSD-2-Clause, with a `LICENSE` file in the crate's
  folder, since the license asks that its notice travel with the code.
  It is permissive like PostgreSQL's, so the crate stays in the
  repository, and the license checks of Rust projects know it.
- `rust-version` from the workspace. CI builds the crate with exactly
  that toolchain, so the promise is checked without a second one. When
  Tessera raises its toolchain, the crate's least version rises with
  it, in a new minor version.
- The README is the crate's page, as in `relhash`, and every public
  item is documented; a missing comment fails the build.
- Dependencies: on Linux, `perf-event-open-sys` and `libc`; `serde`
  behind the feature `serde`. Nothing else. `anyhow` leaves: a library
  gives a typed error.

Refused:

- Keeping the crate in `tools/` with `publish = false`. Its API would
  not be a surface of a spec, and nothing would keep it fit to publish.
- The license PostgreSQL, the workspace's. It allows the same, but the
  license checks of Rust users rarely list it.
- `MIT OR Apache-2.0`, the habit of Rust crates. Apache-2.0 adds terms
  that PostgreSQL's license does not have; BSD-2-Clause is closer.
- A lower Rust version than Tessera's. Keeping that promise needs a
  second toolchain in CI; it can come when a caller asks for it.

### One API on every target

Was: `open() -> anyhow::Result<Counters>`, a `read()` that panics when
kperf fails, and a `cpu_number()` that returns `usize::MAX` off macOS.

Will be:

- `Counters::open() -> Result<Counters, Error>`,
  `Counters::read(&self) -> Result<Reading, Error>` and
  `Counters::measure(&self, iters, f) -> Result<Reading, Error>`.
- `Error` holds a kind, `ErrorKind`, a message that names the cause
  and the fix, and the system's error as its source where there is
  one. `ErrorKind` is `#[non_exhaustive]`, so a new cause is not a
  breaking change. The kinds and their fixes are listed once, in the
  crate's documentation.
- `cpu_number() -> Option<usize>`: the CPU the calling thread runs on,
  or `None` where the system does not say.
- `Reading` keeps its four public fields, `+`, `+=` and `since`. It
  gains `Serialize` and `Deserialize` only with the feature `serde`.
  It stays exhaustive, so that callers build it with a literal, as
  Tessera's test does; a fifth counter will be a breaking version.
- The API is the same on every target. Where no backend is built,
  `open` fails with `ErrorKind::Unsupported`; the other calls cannot
  be reached without a `Counters`.

Why `read` returns an error: on Linux a pinned group that loses the
PMU, to another pinned user or to the NMI watchdog, reads nothing.
That is a state of the machine, not a mistake of the caller, and a
library does not panic on it. A read is two per block of calls; the
cost of a `Result` is a branch. On macOS a read fails only when its
system call fails, as when another process has set kperf up since
`open`; the section "kperf on Intel" says why the kernel lets it.

A block that reads zero instructions or cycles on Apple silicon,
although its code ran, is not an error of the crate. Such reads are
rare, their cause is not known, and the zero is their only sign. The
documentation of `measure` and the README say to take a zero as a
failed block and repeat it, as Tessera's benchmark runner does.

The errors of kperf are told apart by `errno`. Every `kpc_*` call on
the path of `open` returns -1 when it fails and leaves its cause in
`errno`, which the error keeps as its source,
`io::Error::last_os_error()`: `EPERM` from a process that is not root
is `ErrorKind::Permission`; `EACCES` or `EBUSY` from a root process is
`ErrorKind::Busy`, the counters kept for another user, though XNU's
source returns neither for a second process. The `kpep_*` calls return
codes of their own, which the message names.

Refused:

- `anyhow::Error`. A caller cannot match on its cause.
- A `read` that panics. It ends a whole benchmark for one lost block.
- A `measure` that fails with `ErrorKind::Lost` on a zero. It would
  judge only `measure`, not the difference of two reads a caller takes
  itself, so the documentation would still have to warn of zeros; and
  every caller would handle as an error a block it repeats today.
- An API behind `cfg`, present only where a backend is. Every caller
  would repeat the `cfg`.
- A `#[non_exhaustive]` `Reading`. It forbids the literal and the
  struct update outside the crate, and a crate of version 0 may break
  when a counter is added.

### One thread on every platform

Was: `Counters` is `Send` and `Sync` because its fields happen to be.

Will be: `Counters` holds a marker that is neither `Send` nor `Sync`.
It counts the thread that opened it and is read on that thread. On
Linux a file descriptor of an event counts the thread that opened it,
and a read on another thread would silently return the first thread's
counts. A test that does not compile shows that the type cannot be
sent. Every caller in Tessera opens and reads on one thread.

Refused:

- A group per thread, opened on the first read of that thread: an
  open could then fail in the middle of a measurement.
- A check of the thread id on every read: a system call on each read,
  and an error at run time for what the compiler can refuse.

### The Linux backend: one group of the calling thread

Will be: `perf_event_open(2)` opens four events on the core PMU (see
the next section) as one group; the instruction counter leads it.

- One `read(2)` of the leader returns all four, taken at one instant.
- `pid = 0, cpu = -1, inherit = 0`: the calling thread, on any CPU,
  without its children.
- `exclude_kernel` and `exclude_hv`: user space only, as on macOS.
  Cycles are then the unhalted cycles of the core while the thread ran
  in user mode, the meaning `CORE_ACTIVE_CYCLE` has on macOS.
- The leader is `pinned`. A pinned group that cannot stay on the PMU
  goes into an error state, and a read of it returns nothing; it is
  never multiplexed. The read format adds the time the group was
  enabled and the time it ran. A read that returns nothing, or whose
  two times differ, fails with `ErrorKind::Lost`.
- The leader starts disabled. After the group is complete, one `ioctl`
  resets and one enables the whole group.
- The structure `perf_event_attr`, the number of the system call and
  the codes of the `ioctl`s come from `perf-event-open-sys`; `read`,
  `close` and `sched_getcpu` come from `libc`. `cpu_number` calls
  `sched_getcpu`.
- `open` maps each error to its kind. `ENOENT` or `EOPNOTSUPP` for a
  hardware event: no PMU, as in a virtual machine without a virtual
  PMU. `EACCES` or `EPERM`: a setting of `kernel.perf_event_paranoid`
  that refuses the user. `ENOSYS`: no `perf_event_open`, as under
  QEMU's emulation of another processor.
- No root is needed while `kernel.perf_event_paranoid` is 2 or less,
  the kernel's default: any user may count its own threads in user
  space. Debian refuses at 3, its default. Ubuntu refuses at 4, its
  default, and at 3 still lets a user count in user space, as GitHub's
  Arm runner showed. Where the setting refuses, root lowers it to 2,
  or gives the program `CAP_PERFMON` where the kernel accepts it. The
  message says so.

Refused:

- Reading the counters with `rdpmc`, through the page that
  `perf_event_open` can map. A read is two per block of calls, and a
  `read(2)` is one system call, as `kpc_get_thread_counters` is on
  macOS. `rdpmc` adds a retry loop, assembly and unsafe code, and on
  Arm it needs a setting that only root can change. It is worth it
  only if a measurement shows that the cost of a read matters.
- Scaling a multiplexed count by the times. A scaled count is an
  estimate, and a measurement without counters is an error.
- The ABI declared by hand. `perf-event-open-sys` is generated from
  the kernel's headers for every architecture, depends on `libc` alone
  and is widely used; a copy by hand would be one more thing to keep
  right.
- The crate `perf-event`. It has no raw events, which AArch64 needs,
  and its next version breaks its API.

### The core PMU and its four events

Will be: `open` finds the core PMU, the PMU of the processor's cores,
in `/sys/bus/event_source/devices`, as the `perf` tool does: the
device `cpu`, or every device with a file `cpus` that lists its CPUs.
The devices of the rest of the chip have no such file.

- x86-64: the type `PERF_TYPE_HARDWARE` with the generic ids:
  instructions, CPU cycles, branch instructions and branch misses. The
  kernel maps them to retired events, on Intel and on AMD alike. On a
  processor with two kinds of cores, the type of the chosen PMU goes
  into the high 32 bits of each id, as the kernel asks.
- AArch64: raw events of the Arm PMU architecture, on the type of the
  chosen PMU: `INST_RETIRED` (0x08), `CPU_CYCLES` (0x11), `BR_RETIRED`
  (0x21) and `BR_MIS_PRED_RETIRED` (0x22). The kernel's generic ids
  are not used. It maps branch misses to `BR_MIS_PRED`, which counts
  speculative mispredictions, and branches to an event whose meaning
  has changed between kernels. The driver lists a PMU's events as
  files under `events/`. `open` checks all four before it opens
  anything, and names the one missing (`ErrorKind::MissingEvent`): the
  driver opens a raw event the core lacks, or that a hypervisor hides
  from its guest, and it then counts nothing. Some Arm cores lack the
  branch events, such as the Cortex-A72 of a Raspberry Pi 4. A PMU of
  another kind, such as that of Apple's cores under Asahi Linux, is
  refused with `ErrorKind::Unsupported`.
- The choice takes the root of the sysfs tree, the thread's list of
  CPUs and the architecture as arguments; only `open` fills them from
  the running system. The tests of both architectures then run on any
  host, over trees of files written for the test.

Refused: raw codes of Intel events. The kernel already maps the
generic ids, and the same ids then count on AMD.

### Two kinds of cores

Will be: when more than one core PMU is listed, as on Intel since the
12th generation (`cpu_core`, `cpu_atom`) or on Arm boards with big and
little cores, `open` reads the CPUs the calling thread may run on:
`Cpus_allowed_list` of `/proc/thread-self/status`. If they all belong
to one core PMU, the group opens on it. If not, `open` fails with
`ErrorKind::MixedCores`, and names each kind with its CPUs and a
command that binds the process to one, `taskset -c <cpus> <command>`.
An event bound to one kind of core does not count while its thread
runs on the other kind, and the kernel says nothing. This is why the
binding is required, not advised.

macOS differs. It has no call that binds a thread to one kind of
core, and an M5 Pro has two kinds, six "Super" cores and twelve
"Performance" ones. kperf programs the same events on every core, so a
thread is counted on whichever kind it runs, and a reading sums both.
The instructions and the branches do not depend on the kind of core;
the cycles and the branch misses do. The documentation of `Reading`
says so, and a caller that needs to know where a block ran asks
`cpu_number` at both ends of it, as Tessera's benchmark programs do.

Refused:

- A group for each kind of core on Linux, the readings added up, as
  macOS does by itself. It would give Linux the meaning of the macOS
  cells, at the cost of two groups read at two instants. The
  maintainer chose the binding, which keeps cycles in one unit where
  the system allows it.
- Binding the thread from inside `open`. A library that changes the
  affinity of its caller surprises it; the caller binds.

### kperf on Intel

Was: one list of event names, the Apple ones, used on every Mac.

Will be: the same list on Intel, as the check as root agreed. The
kperf database of the Intel Mac at hand resolves the four names to
programmable counters with the user-only bit set, as the proposal
says, and gives no fixed counter for them. Programmable counters
honour the flag that leaves out the kernel, and the test that the
kernel's work is not counted shows it: as root on the Intel Mac a call
of `getppid` counts 12 instructions beyond the loop, as on the M5.

On the M5 the same four names resolve to the Arm architecture's
common events `INST_RETIRED`, `CPU_CYCLES`, `BR_MIS_PRED_RETIRED` and
`BR_RETIRED`, with the bit that counts user space only, and so do the
databases of the M4. These are the events the Linux backend programs
on AArch64, so both NEON cells count the same events. The databases of
the M1 to the M3 resolve the names to Apple's own events, of the same
meaning: retired instructions, cycles, retired branches and retired
mispredictions.

Under Rosetta, an x86-64 program on Apple silicon reads an Intel
family of processor, and `kpep_db_create` finds no database for it.
That refusal is an accident; the counts under Rosetta would be those
of the translated code. `open` checks `sysctl.proc_translated` before
anything else, which needs no root, and fails with
`ErrorKind::Unsupported`, naming Rosetta.

kperf programs the counters of the whole system, not of a thread. A
second setup while another thread of the process counts would
reprogram them under that thread's block. So the first `open` that
succeeds sets kperf up once for the process, and every later `open`, on
any thread, counts with that setup; a setup that fails is not kept, so
a later `open` tries again. The process then holds the counters until
it exits, and the documentation of `Counters` says so. A release when
the last `Counters` is dropped is refused: it would need a count of
the open ones across threads, and a release under a thread that opens
again would race with it.

The kernel does not keep a second process off the counters. In XNU's
source kpc is a layer over the kernel's CPC, whose
`cpc_task_set_owner` finds that another task holds the counters and
still returns success, and `kpc_set_config` reprograms them in any
case. So a second process that sets kperf up spoils the counts of the
first. As root on the Intel Mac, a read of the first then failed with
`EINVAL`, which is `ErrorKind::Lost`, and one of two equal loops
counted 1.45 times the instructions of the other. The documentation of
`Counters` and the README say to count with one program at a time, and
not beside Instruments or `xctrace`.

Refused:

- A list of Intel names now. The database gives the same configuration
  words for them; a second list would add nothing but a list to keep.
- A lock file that `open` holds for the life of the process, so that a
  second program on `pmu` fails with `ErrorKind::Busy`. It would keep
  off only programs on `pmu`, not Instruments or `xctrace`, and the
  person who starts the programs knows what runs.

### Tests that say whether they may skip

Was: a test that cannot open the counters prints the reason and
passes.

Will be: the same, unless `PMU_REQUIRE` is set.

- `software`: on Linux, a test of the plumbing must open a group of
  software events (task clock, context switches). It runs the same
  code as the hardware group: the structure, the group, the `ioctl`s
  and the format of a read.
- `hardware`: every test must also open the hardware counters.
- Any other value fails the test, so that a misspelt name does not
  skip.

A test of the errors runs when `PMU_EXPECT_ERROR` names a kind: `open`
must fail with that kind. CI sets it where it knows the machine.

The examples of the documentation that open the counters are built but
not run (`no_run`). A doctest cannot skip, so it would fail on every
machine without counters; and as root, rustdoc's doctests run as
processes at once and would race for the counters, which only one
task holds.

### A test that the kernel's work is not counted

Will be: a test calls a cheap system call, `getppid`, many times. In
user space a call costs a few instructions; in the kernel it costs
hundreds or more. The loop of the calls is measured alone and taken
away, since in a debug build it costs about a hundred instructions
itself. Beyond it a call counts 12 instructions on the Arm runner, on
the M5 and on the Intel Mac, and about 800 with the kernel on the Arm
runner; the test fails above 100, between the two. It runs on every
backend wherever the hardware counters open: in CI on the Arm runner,
and as root on both Macs.

### CI: the crate alone

Will be: a job like that of `relhash`, on three runners.

- `ubuntu-24.04`, x86-64 without a PMU: the crate's tests with
  `PMU_REQUIRE=software` and `PMU_EXPECT_ERROR=no-counters`. Then a dry
  run of publishing, the public API against its snapshot, and Clippy
  for the four targets of the cells and for a target without a
  backend.
- `ubuntu-24.04-arm`: first the crate's tests at the image's own
  `kernel.perf_event_paranoid`, 4, with `PMU_EXPECT_ERROR=permission`.
  Then the job lowers it to 2 and runs the tests with
  `PMU_REQUIRE=hardware`, the kernel test among them: perf on AArch64
  with real counts.
- `macos-15`: the crate's tests. The virtual machine has no kpc, and
  the tests print why they skip. Then the tests for
  `x86_64-apple-darwin` under Rosetta, with
  `PMU_EXPECT_ERROR=unsupported`: the refusal of Rosetta needs no
  root, so CI checks it at every push.

Refused: `macos-15-intel`. It is a virtual machine too, and the build
for x86-64 macOS that it would add, Clippy for that target gives
already.

### Two steps, two pull requests

The first step brings the crate of its own, its API, the tests that
say whether they skip, the Linux backend and the job of CI. The second
brings the checks as root on both Macs, and closes the change.

## Checks with real counts

No speed is measured in this change. Real counts check that a cell
counts what it should:

- perf on AArch64: the Arm runner of CI, at every push.
- kperf on AArch64: as root on the M5. The example counts as the
  README says, and the tests pass with `PMU_REQUIRE=hardware`, the
  kernel test among them.
- kperf on x86-64: as root on the Intel Mac, the same. The example's
  counts there go into the README.
- perf on x86-64: no hardware at hand. The plumbing runs on software
  events in CI, and the choice of events is tested over written trees.

The maintainer types each command as root on the Mac named. The tests
are built as the maintainer's user, and only their binaries run as
root, through Cargo's runner of the target, after `sudo -v`, such as
`CARGO_TARGET_AARCH64_APPLE_DARWIN_RUNNER='sudo -n env PMU_REQUIRE=hardware'`.
So `target/` gets no files owned by root, and a binary run after
sudo's time is out fails instead of asking for the password again.

The test of two kinds of cores on a Mac measures the same loop on a
thread of the user-interactive class and on a thread of the background
class, which macOS tends to run on different kinds of cores. Their
instructions must be equal within the cost of the reads; the test logs
`cpu_number` at both ends of each. No call of macOS puts a thread on a
chosen kind, and none maps the number of a CPU to its kind, so the
test shows that counting goes on wherever the thread runs, not which
kind it ran on.

Before the Linux backend was written, a probe on a branch that is
never merged asked the Arm runner of CI what its PMU offers; a guest's
PMU shows only what the hypervisor allows. On a Neoverse N2 with Linux
6.17, the runner has one core PMU, `armv8_pmuv3_0`, with six
programmable counters and the cycle counter, and no NMI watchdog. Its
`events/` lists all four events. A pinned group of the four, opened as
the backend will open it, reads with equal times enabled and running.
A loop of ten times the work counts ten times the instructions, and
its branch on random bits mispredicts half the time, as it should. A
call of `getppid` counts 15 instructions in user space, loop included,
and about 800 with the kernel, so the kernel test's bound can sit near
100 on that runner.

## What we do not do

- No fallback to time or to software events when the hardware counters
  cannot open.
- No multiplexed or scaled counts.
- No `rdpmc` until a measurement asks for it.
- No sum over two kinds of cores on Linux, and no binding from inside
  `open`.
- No counting of other threads or of child processes.
- No Linux under Asahi, and no counting under Rosetta.
- No publishing on crates.io in this change.

## Risks / Trade-offs

- x86-64 Linux is not counted on hardware in this change. → The
  software group runs the same code but for the type and the id of
  each event; the choice of events is tested on written trees; the
  error of a missing PMU runs in CI. The check on hardware becomes an
  entry of the roadmap, and the README asks the first user with such a
  machine to run the tests with `PMU_REQUIRE=hardware`.
- The binding to one kind of core is checked on no such processor. →
  The parsing and the choice are tested on written trees.
- GitHub may stop giving its Arm runner a PMU. → The job fails with
  `PMU_REQUIRE=hardware` instead of skipping, so the cell is seen to
  lose its check.
- A new image of the Arm runner may hide an event, give fewer
  counters, or change `kernel.perf_event_paranoid`. → The probe showed
  all four events and six counters; the job with
  `PMU_REQUIRE=hardware` shows any loss as a failure.
- On a Mac with two kinds of cores, cycles and branch misses sum both
  kinds. → The documentation of `Reading` says so; `cpu_number` tells
  where a block began and ended.
- The kernel test on Intel shows the kernel's work counted. → The work
  stops for the maintainer.
- On Intel, kperf may give fewer than four programmable counters, or
  the power manager may hold them. → `open` fails with the code of the
  call, and the check on the Intel Mac shows it.
- The NMI watchdog of Linux holds a counter. → On Intel the group
  needs two fixed counters and two programmable ones, or three
  programmable ones if the watchdog holds the fixed cycle counter; it
  still fits. If not, the pinned group fails instead of multiplexing,
  and the message names `kernel.nmi_watchdog`.
- `perf-event-open-sys` often changes its major version, as it follows
  the kernel's headers. → `Cargo.lock` pins it; a new major version is
  taken when the crate needs a field that only it has.
- Tessera's toolchain is new, so the crate needs a new Rust. → Callers
  on an older Rust cannot take it; a lower version can be promised
  later, with a second toolchain in the crate's job.
