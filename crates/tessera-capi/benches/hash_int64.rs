//! int8 key hashes over selected rows: the kernels and an independent
//! scalar loop.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a benchmark program stops on a broken fixture by panicking"
)]
#[path = "support/hashing64.rs"]
mod hashing64;
#[path = "support/reading64.rs"]
#[allow(dead_code)]
mod reading64;
mod support;

fn main() -> anyhow::Result<()> {
    support::runner::main(hashing64::bench)
}
