//! The table's concurrent protocol under loom: participants append records
//! to chunks of their own, then link them into one index at once, each
//! with a compare-and-swap of its bucket's head, and a probe that reads a
//! head with acquire sees the whole record.
//!
//! The models run the table's own generic code over the memory of
//! `memory.rs`, a second `Region` over loom's atomics, with the orderings
//! that `RawRegion` runs with. The header's fields and the buckets are
//! loom atomics, one per field or bucket; the chunks are plain bytes, and
//! every access to a record or to a chunk's used mark goes through a loom
//! cell of it first, so that loom reports a read not ordered after its
//! writing. The models of the table (`table.rs`) append, link and probe
//! over it; a shared Bloom filter (`filter.rs`) and the words that
//! participants change with an atomic OR, a filter filled together and
//! the marks of a RIGHT or FULL join (`words.rs`), get the same
//! treatment. Run with `make rust-loom`.
//!
//! [`ModelTable`] is that memory with the four steps a shared table takes,
//! for the loom models of the code that shares one, as the phases of a
//! build and the rounds over a partition do. Only a build with
//! `--cfg loom` has this module.

mod memory;
mod model;

pub use model::ModelTable;

#[cfg(test)]
mod filter;
#[cfg(test)]
mod table;
#[cfg(test)]
mod words;
