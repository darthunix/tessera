//! int8 aggregates over selected rows: the kernels and an independent scalar sum.
#[path = "support/aggregating64.rs"]
mod aggregating;
#[path = "support/reading64.rs"]
#[allow(dead_code)]
mod reading64;
mod support;

fn main() -> anyhow::Result<()> {
    support::runner::main(aggregating::bench)
}
