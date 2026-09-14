//! Scalar int32 filtering: independent reference and bounded, fresh row masks.
#[path = "support/filtering.rs"]
mod filtering;
mod support;

criterion::criterion_group! {
    name = benches;
    config = support::criterion();
    targets = filtering::bench
}
criterion::criterion_main!(benches);
