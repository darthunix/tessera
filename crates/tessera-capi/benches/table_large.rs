//! Probes of a hash table far larger than the caches, against a chained
//! table with plain stores.
mod support;
#[path = "support/tabling_large.rs"]
mod tabling_large;

fn main() -> anyhow::Result<()> {
    support::runner::main(tabling_large::bench)
}
