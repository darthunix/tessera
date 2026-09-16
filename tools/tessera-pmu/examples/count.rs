//! Count instructions, cycles and branch misses of an arbitrary closure.
//!
//! Needs root on macOS: `sudo -v && sudo -n cargo run -p tessera-pmu --example count`
//! (or run the built example binary under `sudo -n`).

use anyhow::Result;
use std::hint::black_box;
use tessera_pmu::Counters;

fn main() -> Result<()> {
    let counters = Counters::open()?;
    let values: Vec<u32> = (0..1024).collect();
    let iters = 100_000;
    // Warm up caches and predictors, then count.
    counters.measure(iters, || values.iter().map(|&v| v as u64).sum::<u64>());
    let reading = counters.measure(iters, || {
        black_box(&values).iter().map(|&v| v as u64).sum::<u64>()
    });
    println!(
        "summing 1024 u32 values, {iters} calls on cpu {}:",
        tessera_pmu::cpu_number()
    );
    println!(
        "  per call: {:.2} instructions, {:.2} cycles, {:.3} branch misses, {:.2} branches",
        reading.instructions as f64 / iters as f64,
        reading.cycles as f64 / iters as f64,
        reading.branch_misses as f64 / iters as f64,
        reading.branches as f64 / iters as f64,
    );
    Ok(())
}
