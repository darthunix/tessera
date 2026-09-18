//! Aggregates over selected rows: the kernels and an independent scalar sum.
#[path = "support/aggregating.rs"]
mod aggregating;
#[path = "support/reading.rs"]
#[allow(dead_code)]
mod reading;
mod support;

fn main() -> anyhow::Result<()> {
    support::runner::main(aggregating::bench)
}
