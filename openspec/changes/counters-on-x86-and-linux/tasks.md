## 0. How the work goes

Every task is one commit on the branch `counters-on-x86-and-linux`.
Sections 1 to 4 are the first step and reach `main` in a first pull
request; sections 5 and 6 are the second step, in a second. Each
commit passes `make rust-check` and `.github/scripts/check-specs.sh`;
the check named in a task comes on top. "In CI" means the job of the
crate alone on the pushed branch, with the id of its run in the pull
request. "As root" means a command the maintainer types on the Mac
named.

## 1. The proposal

- [x] 1.1 Write the proposal, the design, the spec, the contract and
      the tasks of this change. Check:
      `openspec validate counters-on-x86-and-linux --strict`;
      `.github/scripts/check-specs.sh`

## 2. A crate of its own

- [ ] 2.1 Move `tools/tessera-pmu` to `crates/pmu` and rename the
      package `pmu`; the workspace and `crates/tessera-capi` follow
      the rename, and nothing else changes. Check: `make rust-check`
- [ ] 2.2 The manifest and the page of a published crate:
      `publish = true`, version 0.1.0, BSD-2-Clause with its
      `LICENSE`, the README as the crate's page, a build that fails on
      a public item without a comment, `serde` as a feature, no
      `anyhow`. Link the spec's requirements to the crate's items.
      Check: `cargo publish --dry-run --locked -p pmu`;
      `RUSTDOCFLAGS=-Dwarnings cargo doc --locked -p pmu --no-deps`
- [ ] 2.3 One API on every target: `Error` and `ErrorKind`, `read` and
      `measure` that return them, a `cpu_number` that returns an
      `Option`, and `Counters` neither `Send` nor `Sync`; the
      benchmark programs read with `?`. Check: a `compile_fail`
      doctest that sends `Counters` to another thread;
      `cargo clippy --locked -p pmu` for the four targets of the cells
      and for a target without a backend
- [ ] 2.4 Read `PMU_REQUIRE` and `PMU_EXPECT_ERROR` in the crate's
      tests, and add the test that the kernel's work in a system call
      is not counted. Check: the tests pass; with
      `PMU_REQUIRE=hardware` they fail without counters, and with an
      unknown value they fail everywhere

## 3. The Linux backend

- [ ] 3.1 The choice of the core PMU: the lists of CPUs, the core PMUs
      of sysfs, the CPUs of the thread, the four events of x86-64 and
      of AArch64, and the check of the Arm events. Check: unit tests
      over written sysfs trees of one PMU, of two kinds of cores bound
      and unbound, of an Arm PMU with and without `br_retired`, and of
      a PMU of another kind
- [ ] 3.2 The group on `perf_event_open`, through
      `perf-event-open-sys`: the group, the `ioctl`s, the read and its
      check, the errors, `cpu_number`, and the test of the plumbing on
      software events. Write the Linux part of the README: the cells,
      the permissions, the binding, the errors and their fixes. Check:
      the crate's tests with `PMU_REQUIRE=software` on both Linux
      runners in CI

## 4. CI

- [ ] 4.1 The job of the crate alone, as the design says, and the
      snapshot of the public API beside this change; the pending
      checks of the contract's "Free of Tessera" and "A small surface"
      name its steps. Check: the job passes in CI, on the Arm runner
      with `PMU_REQUIRE=hardware`, which also gives the first bound of
      the kernel test

## 5. Real counts on the Macs

- [ ] 5.1 kperf on Apple silicon, as root on the M5: the example
      counts as the README says, and the tests pass with
      `PMU_REQUIRE=hardware`. Check: the log in the pull request
- [ ] 5.2 kperf on Intel, as root on the Intel Mac: the same, and the
      example's counts there go into the README. Set the bound of the
      kernel test from the three machines. If the kernel's work is
      counted, stop and ask. Check: the log in the pull request

## 6. Closing

- [ ] 6.1 Close the change as `CONTRIBUTING.md` says: the spec and the
      contract go to `openspec/specs/pmu/`, and what the design
      decided to its `design.md`. The roadmap loses this entry and
      gains three: the next stage (`tessera-bench` on Linux without
      `sudo` and with the cell in its report, a run of
      `tessera-bench` on the Intel Mac, the texts that say the
      counters work only on macOS); the check of x86-64 Linux and of
      two kinds of cores on hardware; and publishing the crate. Check:
      `.github/scripts/check-specs.sh`; the maintainer approves the
      outcome
