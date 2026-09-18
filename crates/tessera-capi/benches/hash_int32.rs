//! Key hashes over selected rows: the kernels and an independent scalar loop.
#[path = "support/hashing.rs"]
mod hashing;
#[path = "support/reading.rs"]
#[allow(dead_code)]
mod reading;
mod support;

fn main() -> anyhow::Result<()> {
    support::runner::main(hashing::bench)
}
