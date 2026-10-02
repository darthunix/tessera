//! Selected-column reading: two library paths and an independent scalar sum.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a benchmark program stops on a broken fixture by panicking"
)]
#[path = "support/reading.rs"]
mod reading;
mod support;

fn main() -> anyhow::Result<()> {
    support::runner::main(reading::bench)
}
