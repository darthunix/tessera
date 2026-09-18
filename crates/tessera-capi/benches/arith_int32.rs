//! Arithmetic over selected rows: the kernels and an independent scalar loop.
#[path = "support/arithmetic.rs"]
mod arithmetic;
#[path = "support/reading.rs"]
#[allow(dead_code)]
mod reading;
mod support;

fn main() -> anyhow::Result<()> {
    support::runner::main(arithmetic::bench)
}
