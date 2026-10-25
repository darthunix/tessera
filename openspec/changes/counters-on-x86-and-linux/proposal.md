## Why

`tools/tessera-pmu` counts four numbers of one thread: retired
instructions, core cycles in user space, retired branches and
mispredicted branches. `tessera-bench` judges every kernel by them.
Today the crate counts only with kperf, the private frameworks of
macOS, on Apple silicon. On Linux `Counters::open` fails with "PMU
counters are implemented only for macOS (kperf)".

The maintainer set three goals on 2026-10-24: AVX2 kernels beside the
NEON ones; a tool for developers on Linux to count what those kernels
cost; and counting in two dimensions, the system and the vector set,
so that a developer in any of the four cells can write a kernel and
see its cost:

```
            NEON (AArch64)              AVX2 (x86-64)
  kperf     macOS, Apple silicon        macOS, Intel
  perf      Linux, AArch64              Linux, x86-64
```

On 2026-10-25 the maintainer added that the crate becomes a crate of
its own, `pmu`, that may be published on crates.io, so that people
outside Tessera can count in the four cells too. No crate on crates.io
does this today:

- `perf-event` counts only on Linux and has no raw events, which Arm
  needs for the right counts of branches.
- `darwin-kperf` counts only on Apple silicon, and counts the kernel's
  work too.
- `kperf` and `kperf-rs` link the private frameworks at build time and
  were never checked on Intel.

What was found on 2026-10-24 and 2026-10-25:

- The Intel Mac at hand (Core i5-1038NG7, Ice Lake, macOS 26). Asked
  without root, the kperf event database of this processor resolves
  the four names the crate uses today to programmable counters, with
  the bit that leaves out the kernel. Their configuration words are
  `0x4100c0`, `0x41003c`, `0x4100c4` and `0x4100c5`: the Intel events
  of retired instructions, core cycles, retired branches and retired
  mispredicted branches. The Intel names and their `_P` twins give the
  same words. So the kperf backend most likely counts on Intel as it
  is; a run as root has to show it.
- No virtual machine on a Mac has a PMU, the unit of the processor
  that counts: neither Docker Desktop's nor QEMU's on Apple's
  hypervisor. A Linux guest there checks the build and the errors, and
  never counts.
- GitHub's runners: `ubuntu-24.04` (x86-64) has no PMU.
  `ubuntu-24.04-arm` counts cycles, with `kernel.perf_event_paranoid`
  at 2. The macOS runners are virtual machines whose kernels have no
  kpc. The repository is public, so the Arm runner costs nothing.
- No Linux machine on x86-64 with a PMU is at hand.

## What Changes

- **BREAKING** for Tessera's benchmark programs: `tools/tessera-pmu`
  becomes `crates/pmu`, the package `pmu`, with `publish = true`, a
  version of its own, 0.1.0, and the license BSD-2-Clause. Its least
  Rust version is the workspace's. The programs follow the rename and
  the new API in commits of their own.
- One public API on every target, documented completely in the crate
  itself. `Counters::open`, `read` and `measure` return a typed error,
  `Error`, whose kind says the cause and whose message says the fix;
  `anyhow` leaves the crate. `cpu_number` returns an `Option`. `serde`
  becomes a feature. `Counters` counts the thread that opened it and
  cannot be sent to another.
- A Linux backend on `perf_event_open`, through the crate
  `perf-event-open-sys`, for x86-64 and AArch64: the four counters as
  one group of the calling thread, in user space only. On AArch64 the
  branches are the architectural events of the Arm PMU, retired
  branches and retired mispredictions. No root is needed where
  `kernel.perf_event_paranoid` is 2 or less, or with `CAP_PERFMON`.
- A processor with two kinds of cores is counted when the thread is
  bound to one kind; the error of an unbound thread names the command
  that binds it.
- On an Intel Mac the kperf backend is checked as root, with a test
  that the kernel's work is not counted. Its list of events changes
  only if that check fails.
- The crate's tests say why they skip, and `PMU_REQUIRE` makes a run
  fail instead.
- A job of CI, the crate alone, on three runners: `ubuntu-24.04`,
  where the plumbing is checked on software events, with the error of
  a missing PMU; `ubuntu-24.04-arm`, with real counts; `macos-15`. It
  also packs the crate as for publishing, builds it for the four cells
  and for a target without a backend, and compares its public API with
  a snapshot.
- Publishing on crates.io is not part of this change: the maintainer
  decides it after the change is closed.

## Capabilities

### New Capabilities

- `pmu`: the public API of the crate `pmu`, a crate of the workspace
  with `publish = true`: what a reading holds, what `open` and `read`
  promise in each cell, the errors, and the crate alone.

### Modified Capabilities

None.

## Impact

- Code: the crate moves to `crates/pmu`. Outside it, the workspace's
  `Cargo.toml` and `crates/tessera-capi` change: its benchmark
  programs and its test follow the rename, read with `?`, and turn a
  missing CPU number into the value they write today.
- Dependencies: `perf-event-open-sys` and `libc`, on Linux only;
  `serde` becomes optional; `anyhow` leaves the crate's dependencies.
- CI: the job of the crate alone.
- Documents: the crate's README, which becomes its page on docs.rs,
  and the link in `crates/tessera-capi/benches/README.md`. The texts
  that say the counters work only on macOS change in the next stage,
  when `tessera-bench` runs on Linux.
- No public header, setting, SQL suite or format changes.
- The roadmap, at the close: this entry leaves; entries for the next
  stage, for the check of x86-64 Linux and of two kinds of cores on
  hardware, and for publishing take its place.
