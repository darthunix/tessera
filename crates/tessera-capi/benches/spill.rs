//! The kernels of a spill: chunks of records and of columns packed and
//! unpacked, rows appended by partition, and a block's header.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a benchmark program stops on a broken fixture by panicking"
)]
#[path = "support/spilling.rs"]
mod spilling;
mod support;

fn main() -> anyhow::Result<()> {
    support::runner::main(spilling::bench)
}
