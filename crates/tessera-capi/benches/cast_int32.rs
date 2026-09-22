//! Widening int4 values into int8 Datums: the kernel and an independent scalar loop.
#[path = "support/casting.rs"]
mod casting;
#[path = "support/reading.rs"]
#[allow(dead_code)]
mod reading;
mod support;

fn main() -> anyhow::Result<()> {
    support::runner::main(casting::bench)
}
