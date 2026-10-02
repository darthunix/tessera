//! The hash table over selected rows: insertion, probes and grouping
//! against a chained table with plain stores.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a benchmark program stops on a broken fixture by panicking"
)]
#[path = "support/reading.rs"]
#[allow(dead_code)]
mod reading;
mod support;
#[path = "support/tabling.rs"]
mod tabling;

fn main() -> anyhow::Result<()> {
    support::runner::main(tabling::bench)
}
