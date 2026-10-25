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
/// in the kernel: on GitHub's Arm runner, a C loop counted 15 in user space,
/// loop included, and about 800 with the kernel. The loop of `measure` is
/// counted alone and taken away, since in a debug build it costs about a
/// hundred instructions itself. Beyond that loop a call counts 12 on the
/// Arm runner, on an M5 Pro and on an Intel Core i5-1038NG7. A bound
/// between the two fails when the kernel's work is counted.
#[cfg(unix)]
#[test]
fn the_kernel_work_of_a_system_call_is_not_counted() {
    const CALLS: u64 = 100_000;
    const BOUND: u64 = 100;
    let Some(counters) = hardware() else { return };
    let empty = counters.measure(CALLS, || 0_u32).unwrap();
    let calls = counters
        .measure(CALLS, std::os::unix::process::parent_id)
        .unwrap();
    let per_call = calls.since(empty);
    eprintln!(
        "getppid: {} instructions, {} cycles, {} branches per call beyond an \
         empty loop of {} instructions",
        per_call.instructions / CALLS,
        per_call.cycles / CALLS,
        per_call.branches / CALLS,
        empty.instructions / CALLS
    );
    assert!(
        calls.instructions > empty.instructions,
        "the calls counted no more than an empty loop: {calls:?}, {empty:?}"
    );
    assert!(
        per_call.instructions / CALLS < BOUND,
        "{} instructions per getppid: the kernel's work is counted",
        per_call.instructions / CALLS
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

/// A loop of known work, the same each call.
fn work() -> u64 {
    (0..10_000_u64).fold(0, |sum, i| black_box(sum ^ i))
}

/// Another thread opens counters again and again while this one
/// measures: the counts of this thread do not change. On macOS a second
/// setup would reprogram the counters under the block.
#[test]
fn a_second_thread_that_opens_leaves_the_first_ones_counts() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let Some(counters) = hardware() else { return };
    let alone = counters.measure(100, work).unwrap();
    let done = AtomicBool::new(false);
    let beside = std::thread::scope(|scope| {
        scope.spawn(|| {
            while !done.load(Ordering::Relaxed) {
                drop(black_box(Counters::open()));
            }
        });
        let beside = counters.measure(100, work);
        done.store(true, Ordering::Relaxed);
        beside
    })
    .unwrap();
    let ratio = beside.instructions as f64 / alone.instructions as f64;
    assert!(
        (0.99..1.01).contains(&ratio),
        "alone {alone:?}, beside an opening thread {beside:?}"
    );
}

/// On a Mac with two kinds of cores, a thread of the user-interactive class
/// and one of the background class tend to run on different kinds; the same
/// loop counts the same instructions on both. macOS cannot put a thread on
/// a chosen kind, so this shows that counting goes on wherever a thread runs.
#[cfg(target_os = "macos")]
#[test]
fn a_loop_counts_the_same_instructions_on_either_kind_of_core() {
    #[allow(unsafe_code)]
    unsafe extern "C" {
        fn pthread_set_qos_class_self_np(class: u32, priority: i32) -> i32;
    }
    const QOS_CLASS_USER_INTERACTIVE: u32 = 0x21;
    const QOS_CLASS_BACKGROUND: u32 = 0x09;
    if hardware().is_none() {
        return;
    }
    let on = |class: u32| {
        std::thread::spawn(move || {
            // SAFETY: sets the class of the calling thread; no pointers.
            #[allow(unsafe_code)]
            let set = unsafe { pthread_set_qos_class_self_np(class, 0) };
            assert_eq!(set, 0, "pthread_set_qos_class_self_np");
            let counters = Counters::open().unwrap();
            let first = pmu::cpu_number();
            let reading = counters.measure(1_000, work).unwrap();
            (reading, first, pmu::cpu_number())
        })
        .join()
        .unwrap()
    };
    let (fast, fast_first, fast_last) = on(QOS_CLASS_USER_INTERACTIVE);
    let (slow, slow_first, slow_last) = on(QOS_CLASS_BACKGROUND);
    eprintln!(
        "user-interactive: {fast:?} on cpus {fast_first:?}..{fast_last:?}; \
         background: {slow:?} on cpus {slow_first:?}..{slow_last:?}"
    );
    let ratio = slow.instructions as f64 / fast.instructions as f64;
    assert!((0.99..1.01).contains(&ratio), "ratio {ratio}");
}
