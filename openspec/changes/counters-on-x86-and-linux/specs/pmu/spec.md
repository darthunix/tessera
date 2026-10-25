## ADDED Requirements

### Requirement: A crate of its own
The counters SHALL be the Rust crate `pmu`, which depends on no crate
of Tessera, and SHALL build, pass its tests and pack alone. It SHALL
build for every target, and give the same public API on each.

Its documentation SHALL be complete by itself, as [the crate's
page](../../../crates/pmu/src/lib.rs) maps it: every public item SHALL
say what it is for, what it takes and gives, the errors it returns and
what its caller must do. The documentation SHALL build without warnings.
Every example in it SHALL build as a test, and every example that does
not open the counters SHALL run as one.

#### Scenario: The crate packs alone
- **WHEN** the crate is packed, as for publishing
- **THEN** it builds from the packed files and the crates it names,
  none of them Tessera's
- **Verified by:** pending

#### Scenario: The crate's tests run alone
- **WHEN** the tests of the crate run as the only package, on Linux
  for x86-64 and for AArch64, and on macOS
- **THEN** they pass
- **Verified by:** pending

#### Scenario: A target without counters
- **WHEN** the crate is built for a target where it has no backend
- **THEN** it builds, with the same public items
- **Verified by:** pending

### Requirement: What a reading holds
A [`Reading`](../../../crates/pmu/src/lib.rs) SHALL hold four counts of
the thread that opened the `Counters`: retired instructions, core
cycles, retired branches and retired mispredicted branches. Each SHALL
count user space only: the work of the kernel for the thread, in system
calls, interrupts and page faults, SHALL NOT be counted.
`Reading::since` SHALL give the counts between two readings, and the sum
of two readings SHALL add each count. With the feature `serde`, a
reading SHALL serialize as its four named counts.

#### Scenario: Ten times the work
- **WHEN** a loop of calls is measured once, then with ten times as
  many calls
- **THEN** the second reading counts ten times the instructions of the
  first, within the small fixed cost of the reads
- **Verified by:**
  `crates/pmu/tests/open.rs::counters_count_a_known_loop_or_explain_why_not`

#### Scenario: A system call is not counted
- **WHEN** a thread calls `getppid` many times between two readings
- **THEN** each call counts fewer instructions than the kernel's work
  for it would add
- **Verified by:**
  `crates/pmu/tests/open.rs::the_kernel_work_of_a_system_call_is_not_counted`

#### Scenario: The difference of two readings
- **WHEN** a reading is taken `since` a reading with a larger count
- **THEN** that count is 0, and the others are their differences
- **Verified by:**
  `crates/pmu/src/lib.rs::since_subtracts_per_field_and_saturates`

#### Scenario: A reading as data
- **WHEN** a reading is serialized and read back, with the feature
  `serde`
- **THEN** it is the same reading, and its counts are named
  `instructions`, `cycles`, `branches` and `branch_misses`
- **Verified by:**
  `crates/pmu/src/lib.rs::a_reading_serializes_as_its_four_named_counts`

### Requirement: The thread that opened the counters
[`Counters`](../../../crates/pmu/src/lib.rs) SHALL count the thread that
called `Counters::open`, on whichever cores it runs, and SHALL be read
on that thread. The type SHALL be neither `Send` nor `Sync`, so that the
compiler refuses a read on another thread.

#### Scenario: Counters sent to another thread
- **WHEN** a program moves `Counters` into another thread
- **THEN** it does not compile
- **Verified by:**
  `crates/pmu/src/lib.rs::fn moves(counters: pmu::Counters)`

### Requirement: Exact or an error
[`Counters::read`](../../../crates/pmu/src/lib.rs) and
[`Counters::measure`](../../../crates/pmu/src/lib.rs) SHALL give a
reading only when every counter counted for the whole time since `open`;
otherwise they SHALL fail with `ErrorKind::Lost`. A reading SHALL NOT be
scaled, estimated, or made from time or from software events.

#### Scenario: Counters lost
- **WHEN** the counters of an open group stop counting before a read,
  as when a pinned group of the Linux kernel loses its PMU
- **THEN** the read fails with `ErrorKind::Lost`
- **Verified by:** pending

### Requirement: The four cells
[`Counters::open`](../../../crates/pmu/src/lib.rs) SHALL count on macOS
through kperf, on Apple silicon and on Intel, and on Linux through
`perf_event_open`, on AArch64 and on x86-64. On Linux for AArch64 the
branches SHALL be the Arm PMU's `BR_RETIRED` and `BR_MIS_PRED_RETIRED`.
On any other system or processor, and under Rosetta, `open` SHALL fail
with `ErrorKind::Unsupported`.

#### Scenario: Linux on AArch64
- **WHEN** the tests run on Linux on an AArch64 machine with a PMU
- **THEN** the hardware counters open and count
- **Verified by:** pending

#### Scenario: Linux on x86-64, without a PMU at hand
- **WHEN** a group of software events opens through the code of the
  hardware group, on Linux for x86-64
- **THEN** it resets, counts and reads as the hardware group would
- **Verified by:**
  `crates/pmu/src/linux.rs::a_group_of_software_events_counts_through_the_same_code`

#### Scenario: macOS
- **WHEN** the tests run as root on a Mac with Apple silicon and on a
  Mac with Intel
- **THEN** the hardware counters open and count on both
- **Verified by:** pending

#### Scenario: Under Rosetta
- **WHEN** a build for x86-64 runs under Rosetta on Apple silicon
- **THEN** `open` fails with `ErrorKind::Unsupported`, and the message
  names Rosetta
- **Verified by:** pending

### Requirement: Errors that say what to do
When `Counters::open` fails, its
[`Error`](../../../crates/pmu/src/error.rs) SHALL give a kind,
[`ErrorKind`](../../../crates/pmu/src/error.rs), that names the cause,
and a message that names the fix, with the system's error as its source
where there is one. The documentation of `ErrorKind` SHALL list the
kinds and their fixes. A new kind MAY come in a minor version.

#### Scenario: No PMU
- **WHEN** a Linux machine has no PMU, as a virtual machine without a
  virtual one
- **THEN** `open` fails with `ErrorKind::NoCounters`
- **Verified by:**
  `crates/pmu/src/core_pmu.rs::a_tree_without_a_core_pmu_has_no_counters`

#### Scenario: No permission
- **WHEN** the setting `kernel.perf_event_paranoid` refuses the user
  its own counters, as Ubuntu's default of 4 does; or a process on
  macOS is not root
- **THEN** `open` fails with `ErrorKind::Permission`, and the message
  names the setting and the capability, or root
- **Verified by:** pending

#### Scenario: Counters in use
- **WHEN** a process on macOS holds the counters, and a second process
  opens them, as root
- **THEN** the second `open` fails with `ErrorKind::Busy`, and the
  first process keeps counting
- **Verified by:** pending

#### Scenario: An Arm PMU without an event
- **WHEN** the core PMU of an AArch64 machine lists no `br_retired` or
  no `br_mis_pred_retired`
- **THEN** `open` fails with `ErrorKind::MissingEvent` and names the
  event, before it opens anything
- **Verified by:**
  `crates/pmu/src/core_pmu.rs::an_arm_pmu_without_a_branch_event_is_refused`

### Requirement: Two kinds of cores
When a Linux machine has more than one core PMU, `Counters::open`
SHALL count only if every CPU the thread may run on belongs to one of
them. Otherwise it SHALL fail with `ErrorKind::MixedCores`, and its
message SHALL name each kind with its CPUs, and a command that binds
the process to one. `open` SHALL NOT change the CPUs the thread may
run on.

macOS cannot bind a thread to one kind of core. There a reading SHALL
count on every kind of core the thread ran on, and the documentation
of `Reading` SHALL say that its cycles and branch misses may then mix
kinds, while its instructions and branches do not depend on the kind.

#### Scenario: A bound thread
- **WHEN** a machine has two core PMUs, and the thread may run only on
  the CPUs of one
- **THEN** the counters open on that PMU
- **Verified by:**
  `crates/pmu/src/core_pmu.rs::two_kinds_of_x86_cores_count_on_the_bound_one`;
  `crates/pmu/src/core_pmu.rs::big_and_little_arm_cores_count_on_the_bound_one`;
  `crates/pmu/src/core_pmu.rs::offline_cpus_do_not_spread_a_thread_over_two_kinds`

#### Scenario: An unbound thread
- **WHEN** the thread may run on CPUs of both
- **THEN** `open` fails with `ErrorKind::MixedCores`, and names both
  kinds, their CPUs and `taskset`
- **Verified by:**
  `crates/pmu/src/core_pmu.rs::an_unbound_thread_on_two_kinds_of_cores_is_refused`;
  `crates/pmu/src/core_pmu.rs::a_single_pmu_of_some_cpus_counts_only_a_thread_bound_to_them`

#### Scenario: A Mac with two kinds of cores
- **WHEN** on a Mac whose cores are of two kinds, the same loop is
  measured, as root, on a thread of the user-interactive class and on
  a thread of the background class
- **THEN** both count the same instructions, within the cost of the
  reads
- **Verified by:** pending

### Requirement: The CPU of the thread
[`cpu_number`](../../../crates/pmu/src/lib.rs) SHALL give the CPU the
calling thread runs on, on macOS and on Linux, and `None` on any other
system.

#### Scenario: A CPU of the machine
- **WHEN** a thread asks for its CPU on Linux
- **THEN** the number is below the count of the machine's CPUs
- **Verified by:**
  `crates/pmu/src/linux.rs::the_cpu_of_the_thread_is_a_cpu_of_the_machine`
