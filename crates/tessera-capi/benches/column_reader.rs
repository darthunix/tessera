//! Selected-column reading: three library paths and an independent scalar sum.
#[path = "support/reading.rs"]
mod reading;
mod support;

criterion::criterion_group! {
    name = benches;
    config = support::criterion();
    targets = reading::bench
}
criterion::criterion_main!(benches);
