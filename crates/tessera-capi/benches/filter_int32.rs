//! Scalar int32 filtering: independent reference and bounded, fresh row masks.
#[path = "support/filtering.rs"]
mod filtering;
mod support;

fn main() -> anyhow::Result<()> {
    support::runner::run(
        filtering::DEFINITION,
        filtering::cases(),
        &[
            include_str!("filter_int32.rs"),
            include_str!("support/filtering.rs"),
            include_str!("support/filter_timing.rs"),
        ],
        filtering::measure,
    )
}
