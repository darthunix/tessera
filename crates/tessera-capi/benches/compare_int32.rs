//! Comparisons of two int4 columns: the kernel and an independent scalar loop.
#[path = "support/comparing.rs"]
mod comparing;
mod support;

fn main() -> anyhow::Result<()> {
    support::runner::main(comparing::bench)
}
