//! Count instructions, cycles and branch misses of an arbitrary closure.
//!
//! Needs root on macOS. Build it as yourself, and run only the binary as
//! root: `cargo build --release -p pmu --example count`, then `sudo -v` and
//! `sudo -n target/release/examples/count`.

use pmu::{Counters, Error};
use std::{hint::black_box, process::ExitCode};

fn main() -> ExitCode {
    match count() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn count() -> Result<(), Error> {
    let counters = Counters::open()?;
    let values: Vec<u32> = (0..1024).collect();
    let iters = 100_000;
    // Warm up caches and predictors, then count.
    counters.measure(iters, || values.iter().map(|&v| v as u64).sum::<u64>())?;
    let reading = counters.measure(iters, || {
        black_box(&values).iter().map(|&v| v as u64).sum::<u64>()
    })?;
    println!(
        "summing 1024 u32 values, {iters} calls on cpu {}:",
        pmu::cpu_number().map_or("unknown".to_owned(), |cpu| cpu.to_string())
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
