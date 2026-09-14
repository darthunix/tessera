//! Small building blocks shared by benchmarks and their deterministic tests.
//!
//! Criterion handles sampling, statistics and persistence. Operation modules
//! supply cases and timed functions; revision comparison lives in tessera-bench.
#![allow(dead_code)] // Each target uses only its own operations.

pub mod fixture;
pub mod reference;

pub fn criterion() -> criterion::Criterion {
    criterion::Criterion::default()
        .sample_size(100)
        .warm_up_time(std::time::Duration::from_millis(100))
        .measurement_time(std::time::Duration::from_secs(1))
        .confidence_level(0.99)
        .noise_threshold(0.03)
        .without_plots()
}
