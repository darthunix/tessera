//! int8 key hashes over selected rows: the kernels and an independent
//! scalar loop.
#[path = "support/hashing64.rs"]
mod hashing64;
#[path = "support/reading64.rs"]
#[allow(dead_code)]
mod reading64;
mod support;

fn main() -> anyhow::Result<()> {
    support::runner::main(hashing64::bench)
}
