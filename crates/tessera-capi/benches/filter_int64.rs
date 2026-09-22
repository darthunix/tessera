//! Scalar int64 filtering: independent reference and bounded, fresh row masks.
#[path = "support/filtering64.rs"]
mod filtering;
mod support;

fn main() -> anyhow::Result<()> {
    support::runner::main(filtering::bench)
}
