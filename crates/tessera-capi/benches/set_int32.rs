//! Membership of int4 and int8 values in sets of constants over selected
//! rows: the kernel on sets around its bounds and an independent scalar
//! loop.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a benchmark program stops on a broken fixture by panicking"
)]
#[path = "support/reading.rs"]
#[allow(dead_code)]
mod reading;
#[path = "support/reading64.rs"]
#[allow(dead_code)]
mod reading64;
#[path = "support/sets.rs"]
mod sets;
mod support;

fn main() -> anyhow::Result<()> {
    support::runner::main(sets::bench)
}
