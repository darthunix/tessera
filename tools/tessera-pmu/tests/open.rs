//! Opens the counters when the platform and privileges allow it; otherwise
//! prints the reason and passes, so that a broken root setup is still visible
//! in the output while unprivileged test runs stay green.

use std::hint::black_box;
use tessera_pmu::{Counters, Reading};

#[test]
fn counters_count_a_known_loop_or_explain_why_not() {
    let counters = match Counters::open() {
        Ok(counters) => counters,
        Err(error) => {
            eprintln!("PMU counters unavailable: {error:#}");
            return;
        }
    };
    let short = counters.measure(1_000, || black_box(1_u64) + 1);
    let long = counters.measure(10_000, || black_box(1_u64) + 1);
    assert!(short.instructions > 0 && short.cycles > 0);
    // Ten times the calls take roughly ten times the instructions.
    let ratio = long.instructions as f64 / short.instructions as f64;
    assert!((8.0..12.0).contains(&ratio), "ratio {ratio}");
    assert!(long.branches >= short.branches);
    assert_ne!(counters.read(), Reading::default());
}
