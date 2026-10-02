//! Scalar int32 filtering: independent reference and bounded, fresh row masks.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a benchmark program stops on a broken fixture by panicking"
)]
#[path = "support/filtering.rs"]
mod filtering;
mod support;

fn main() -> anyhow::Result<()> {
    support::runner::main(filtering::bench)
}
