//! Selected-column reading: two library paths and an independent scalar sum.
#[path = "support/reading.rs"]
mod reading;
mod support;

fn main() -> anyhow::Result<()> {
    support::runner::main(reading::bench)
}
