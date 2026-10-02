//! Comparisons of two int4 columns: the kernel and an independent scalar loop.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a benchmark program stops on a broken fixture by panicking"
)]
#[path = "support/comparing.rs"]
mod comparing;
mod support;

fn main() -> anyhow::Result<()> {
    support::runner::main(comparing::bench)
}
