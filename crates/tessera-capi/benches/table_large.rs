//! Probes of a hash table far larger than the caches, against a chained
//! table with plain stores.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a benchmark program stops on a broken fixture by panicking"
)]
mod support;
#[path = "support/tabling_large.rs"]
mod tabling_large;

fn main() -> anyhow::Result<()> {
    support::runner::main(tabling_large::bench)
}
