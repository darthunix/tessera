//! Selected-column reading: three library paths and an independent scalar sum.
#[path = "support/reading.rs"]
mod reading;
mod support;

fn main() -> anyhow::Result<()> {
    support::runner::run(
        reading::DEFINITION,
        reading::cases(),
        &[
            include_str!("column_reader.rs"),
            include_str!("support/reading.rs"),
        ],
        reading::measure,
    )
}
