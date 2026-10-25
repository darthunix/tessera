//! The counters on the machine at hand.
//!
//! A test that cannot open the hardware counters prints why and passes,
//! so that a run without them stays green and still shows the cause.
//! `PMU_REQUIRE` turns the skip into a failure: `hardware` when the
//! counters must open, `software` when only the plumbing must work; any
//! other value fails every test, so that a misspelt name does not skip.
//! `PMU_EXPECT_ERROR` names the kind of error `open` must fail with, on a
//! machine whose answer the run knows.

// A test reports a failure by panicking, from its helpers too, which
// clippy.toml does not count as tests.
#![allow(clippy::panic)]

use pmu::{Counters, ErrorKind, Reading};
use std::{env, hint::black_box};

/// What the run asks of the counters.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Require {
    Nothing,
    Software,
    Hardware,
}

fn require() -> Require {
    match env::var("PMU_REQUIRE") {
        Err(env::VarError::NotPresent) => Require::Nothing,
        Ok(value) if value == "software" => Require::Software,
        Ok(value) if value == "hardware" => Require::Hardware,
        other => panic!("PMU_REQUIRE={other:?}: the values are software and hardware"),
    }
}

/// The hardware counters, or `None` when they cannot open and the run
/// allows a skip.
fn hardware() -> Option<Counters> {
    let require = require();
    match Counters::open() {
        Ok(counters) => Some(counters),
        Err(error) if require != Require::Hardware => {
            eprintln!(
                "hardware counters unavailable, test skipped ({:?}): {error}",
                error.kind()
            );
            None
        }
        Err(error) => panic!(
            "PMU_REQUIRE=hardware, but the counters do not open ({:?}): {error}",
            error.kind()
        ),
    }
}

#[test]
fn counters_count_a_known_loop_or_explain_why_not() {
    let Some(counters) = hardware() else { return };
    let short = counters.measure(1_000, || black_box(1_u64) + 1).unwrap();
    let long = counters.measure(10_000, || black_box(1_u64) + 1).unwrap();
    assert!(short.instructions > 0 && short.cycles > 0);
    // Ten times the calls take roughly ten times the instructions.
    let ratio = long.instructions as f64 / short.instructions as f64;
    assert!((8.0..12.0).contains(&ratio), "ratio {ratio}");
    assert!(long.branches >= short.branches);
    assert_ne!(counters.read().unwrap(), Reading::default());
}

/// A call of `getppid` costs a few instructions in user space and hundreds
/// in the kernel: on GitHub's Arm runner 15 in user space, loop included,
/// and about 800 with the kernel. A bound between the two fails when the
/// kernel's work is counted.
#[cfg(unix)]
#[test]
fn the_kernel_work_of_a_system_call_is_not_counted() {
    const CALLS: u64 = 100_000;
    const BOUND: u64 = 100;
    let Some(counters) = hardware() else { return };
    let reading = counters
        .measure(CALLS, std::os::unix::process::parent_id)
        .unwrap();
    let per_call = reading.instructions / CALLS;
    eprintln!(
        "getppid: {per_call} instructions, {} cycles, {} branches per call",
        reading.cycles / CALLS,
        reading.branches / CALLS
    );
    assert!(per_call > 0, "no instructions counted: {reading:?}");
    assert!(
        per_call < BOUND,
        "{per_call} instructions per getppid: the kernel's work is counted"
    );
}

#[test]
fn open_fails_with_the_kind_the_run_expects() {
    require();
    let name = match env::var("PMU_EXPECT_ERROR") {
        Err(env::VarError::NotPresent) => return,
        Ok(name) => name,
        other => panic!("PMU_EXPECT_ERROR={other:?} is not a name"),
    };
    let expected = match name.as_str() {
        "unsupported" => ErrorKind::Unsupported,
        "no-counters" => ErrorKind::NoCounters,
        "permission" => ErrorKind::Permission,
        "busy" => ErrorKind::Busy,
        "missing-event" => ErrorKind::MissingEvent,
        "mixed-cores" => ErrorKind::MixedCores,
        "other" => ErrorKind::Other,
        _ => panic!(
            "PMU_EXPECT_ERROR={name:?}: the values are unsupported, no-counters, \
             permission, busy, missing-event, mixed-cores and other"
        ),
    };
    match Counters::open() {
        Ok(_) => panic!("PMU_EXPECT_ERROR={name}, but the counters opened"),
        Err(error) => {
            eprintln!("open failed as expected ({:?}): {error}", error.kind());
            assert_eq!(error.kind(), expected, "{error}");
        }
    }
}
