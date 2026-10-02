//! Aggregates over selected rows: the kernels and an independent scalar sum.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a benchmark program stops on a broken fixture by panicking"
)]
#[path = "support/aggregating.rs"]
mod aggregating;
#[path = "support/reading.rs"]
#[allow(dead_code)]
mod reading;
mod support;

fn main() -> anyhow::Result<()> {
    support::runner::main(aggregating::bench)
}
