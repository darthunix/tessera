//! int8 arithmetic over selected rows: the kernels and an independent scalar loop.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a benchmark program stops on a broken fixture by panicking"
)]
#[path = "support/arithmetic64.rs"]
mod arithmetic;
#[path = "support/reading64.rs"]
#[allow(dead_code)]
mod reading64;
mod support;

fn main() -> anyhow::Result<()> {
    support::runner::main(arithmetic::bench)
}
