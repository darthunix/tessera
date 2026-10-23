// The crate's page is its README, which needs the owning table for its
// example.
#![cfg_attr(feature = "alloc", doc = include_str!("../README.md"))]
//! # The parts
//!
//! A table lives in blocks of memory that the caller owns: one index and
//! any number of chunks. The table keeps no address between calls, and
//! every call is given the blocks again, as bases and lengths
//! ([`Chunks`]). So the same table works in the memory of one process
//! and in shared memory that each process maps at an address of its own.
//!
//! ```text
//!   index                       chunks
//!   ┌──────────┐                ┌────────────┐  chunk 0
//!   │ header   │                │ used mark  │
//!   ├──────────┤                ├────────────┤
//!   │ bucket 0 │───────────────►│ record A   │──┐ next
//!   │ bucket 1 │ 0: no record   ├────────────┤  │
//!   │ bucket 2 │───┐            │ record B   │  │
//!   │ …        │   │            ├────────────┤  │
//!   └──────────┘   │            │ free       │  │
//!                  │            └────────────┘  │
//!                  │            ┌────────────┐  │ chunk 1
//!                  │            │ used mark  │  │
//!                  │            ├────────────┤  │
//!                  └───────────►│ record C   │  │
//!                               ├────────────┤  │
//!                               │ record D   │◄─┘
//!                               └────────────┘
//! ```
//!
//! - The **index** is a header and an array of buckets. A row's hash
//!   picks its bucket by its high bits. [`HEADER_SIZE`] draws the
//!   header, and [`index_size`] gives the buckets of an index.
//! - A **chunk** starts with its **used mark**, the bytes in use, and
//!   holds records one after another. [`Chunks`] draws a chunk.
//! - A **record** is one row: its hash, the reference of the next record
//!   of its chain, the NULL bits of its keys, a word of 8 bytes per key,
//!   and a **payload**, bytes the table never reads. A join keeps the row
//!   it returns there, a grouping the states of its aggregates.
//!   [`Record`] draws a record.
//! - A **reference** names a record in 32 bits: its chunk and its place
//!   in the chunk. A bucket holds the reference of the first record of
//!   its **chain**, and each record that of the next one; 0 means none,
//!   since no record starts where a chunk's used mark lies. [`Chunks`]
//!   draws a reference too.
//!
//! A key is a word compared bit for bit: the table gives it no type. The
//! caller computes the hash, so its hash decides which keys meet. The
//! high bits of the hash pick the bucket, so they must differ from key to
//! key: integers taken as their own hashes would put close keys into one
//! chain. A key that a word does not hold, such as a string, goes in as a
//! number that stands for it, as the last example shows.
//!
//! # How rows go in and come out
//!
//! A call takes a batch of rows: each row's hash, its keys through a
//! [`KeySource`], and a mask of the rows it works on ([`RowMask`]).
//!
//! 1. **Append.** The one writer of a chunk writes records at its end and
//!    moves its used mark ([`Table::append`]). The records are not found
//!    yet.
//! 2. **Link.** Each record goes first in its bucket's chain
//!    ([`Table::link`]). Several participants may link at once, each its
//!    own chunks.
//! 3. **Probe.** A row finds the first record of its hash and keys
//!    ([`Table::probe`]), the ones after it ([`Table::next_match`]) and
//!    the words of their payloads ([`Table::gather`]).
//!
//! A table that one writer has to itself is a [`TableMut`]: it may find
//! or create the record of each row's keys, for a grouping
//! ([`TableMut::find_or_insert`]), change payloads in place, and move the
//! table to a larger index ([`TableMut::regrow`]). Records never move: a
//! larger index links the same records again.
//!
//! The arrays a call fills, such as the references of the rows found or
//! the words gathered, are the caller's, a slot per row of the batch.
//! The call writes only the rows of its mask and leaves the other slots
//! as they were. So a call allocates nothing, a node can hand over the
//! column it returns, and several calls can fill one array.
//!
//! # The items
//!
//! - **A table.** [`OwnedTable`] owns its blocks and needs no `unsafe`.
//!   [`Table`] and [`TableMut`] work over blocks the caller gives: an
//!   index of [`index_size`] bytes for a [`TableConfig`], and
//!   [`Chunks`]; the contracts of the `unsafe` calls that make them say
//!   what the caller promises. [`Table::stats`] counts what a table
//!   holds, and constants such as [`MAX_KEYS`], [`MAX_CHUNKS`] and
//!   [`MAX_CHUNK_LEN`] are the limits of its format.
//! - **Rows in.** A [`Batch`] holds the rows to place over one call or
//!   several. [`Table::append`] and [`Table::append_columns`] write
//!   records, [`Table::link`] and [`TableMut::link_grouped`] link them;
//!   [`TableMut::find_or_insert`] finds or makes the record of each
//!   row's keys, [`TableMut::merge`] merges a chunk's records by the
//!   caller's rule, and [`TableMut::regrow`] moves the table to a larger
//!   index.
//! - **Lookups.** A [`Matches`] holds the rows of a lookup and the
//!   record each has reached: [`Table::probe`], [`Table::next_match`],
//!   [`Table::next_in_group`] and the gathers move it on and read it.
//! - **Records and payloads.** [`Table::record`] and [`RecordCursor`]
//!   read a record by its reference, [`PayloadCursor`] changes its
//!   payload; [`PayloadColumns`] gives the payloads of a batch from its
//!   columns, and [`TableMut::each_payload`] and
//!   [`TableMut::payload_ptrs`] change them; [`TableMut::scan`] visits
//!   every record from a [`Cursor`].
//! - **A table that spills.** [`Partitions`] say which chunk the record
//!   of a hash goes to; [`ChunkWriter`] appends to them and splits a
//!   chunk by them, and [`TableMut::find_or_insert_partitioned`] groups
//!   by them.
//! - **For joins.** The [`bloom`] filter of a table's keys, alone or
//!   shared; the [`Marks`] of the records a RIGHT or FULL join matched,
//!   with [`Table::mark`] and [`Table::scan_unmarked`].
//! - **Masks and errors.** [`RowMask`] and [`RowMaskView`]; [`Error`],
//!   [`Kind`] and [`Rule`].
//!
//! # What a call checks
//!
//! The table trusts no byte it reads: under the caller's promise, which
//! the crate's page above states, bytes that are not a table give an
//! error, never a crash or a call that does not end. An error is an
//! [`Error`]: the [`Rule`] broken, its [`Kind`], a wrong call or a
//! damaged table, and up to two numbers, in one word. [`Kind`] says which
//! values make a wrong call and which bytes a damaged table. A full chunk
//! or index is not an error: the rows without room stay in the mask, and
//! the caller adds a chunk or a larger index and calls again. A table a
//! call found damaged is to be dropped or reset.
//!
//! A call over blocks the caller gives allocates no memory, also when it
//! fails: the memory of the table, of the arguments and of the results
//! is the caller's. [`OwnedTable`], which allocates its blocks, is
//! outside this promise.
//!
//! # Processes and threads
//!
//! Several processes may use one table in shared memory, each through a
//! [`Table`] of its own, in the steps its documentation lists. A thread
//! is a participant as a process is: a [`Table`] or a [`TableMut`] is
//! neither `Send` nor `Sync`, so each thread attaches its own handle to
//! the same blocks. [`OwnedTable`], which owns its blocks and whose
//! handles borrow it, may move to another thread whole, and a
//! [`bloom::SharedFilter`], which holds only atomic words, may be shared.
//!
//! # Without the feature `alloc`
//!
//! Without the feature `alloc` the crate has no [`OwnedTable`] and takes
//! no memory at all. The caller gives the blocks: an index of
//! [`index_size`] bytes and chunks that [`Chunks::init`] made empty, and
//! the calls stop when a chunk is full for the caller to add one.
//!
//! ```
//! use relhash::{Batch, Chunks, KeySource, Matches, TableConfig, TableMut, index_size, ones};
//!
//! struct Keys<'a>(&'a [i32]);
//! impl KeySource for Keys<'_> {
//!     fn nkeys(&self) -> usize { 1 }
//!     fn nrows(&self) -> usize { self.0.len() }
//!     fn word(&self, _: usize, index: usize, selected: u64, out: &mut [i64; 64]) -> Option<u64> {
//!         for bit in ones(selected) {
//!             out[bit] = i64::from(self.0[index * 64 + bit]);
//!         }
//!         Some(selected)
//!     }
//! }
//!
//! let config = TableConfig { nkeys: 1, payload_size: 8 };
//! let mut index = vec![0_u64; index_size(&config, 100)? / 8];
//! let mut chunk = vec![0_u64; 4096 / 8];
//! let bases = [chunk.as_mut_ptr().cast::<u8>()];
//! let lens = [4096];
//! // SAFETY: the index and the chunk are these vectors, apart, aligned to
//! // 8 and alive while the table is, and nothing else uses them.
//! let mut table = unsafe {
//!     Chunks::init(bases[0], lens[0])?;
//!     let chunks = Chunks::new(&bases, &lens)?;
//!     TableMut::create(index.as_mut_ptr().cast(), index.len() * 8, &config, 100, chunks)?
//! };
//! let keys = Keys(&[7, 8]);
//! let hashes = [7u32, 8].map(|key| key.wrapping_mul(0x9e37_79b1));
//! let (mut pending, mut refs) = ([0b11], [0; 2]);
//! table.append(0, None, &mut Batch::new(&hashes, &keys, &mut pending, &mut refs)?)?;
//! let mut from = relhash::CHUNK_HEADER;
//! table.link(0, &mut from)?;
//! let (mut rows, mut found) = ([0b10], [0; 2]);
//! let mut matches = Matches::new(&mut rows, &mut found)?;
//! assert_eq!(table.probe(&hashes, &keys, &mut matches)?, 1);
//! assert_eq!(matches.refs()[1], refs[1]);
//! # Ok::<(), relhash::Error>(())
//! ```
//!
//! # Keys that a word does not hold
//!
//! A string or a number of many digits goes in as a number of 64 bits
//! that stands for it. A join can take the value's hash: the table finds
//! the records of the same hash, and since two values may share one, the
//! caller compares the values themselves and walks on to the next match
//! where they differ. The record's payload says where its value lies.
//! A grouping needs one record a value, and [`TableMut::find_or_insert`]
//! takes equal words for equal keys, so a hash would merge the groups of
//! two values that share it: a grouping numbers its values in a
//! dictionary of its own and gives the table the numbers.
//!
//! ```
//! # #[cfg(feature = "alloc")] {
//! use std::hash::{DefaultHasher, Hash, Hasher};
//! use relhash::{KeySource, Matches, OwnedTable, RowMaskView, TableConfig, ones};
//!
//! // Each string's key is its hash of 64 bits, which the table compares bit
//! // for bit. The table's hash of 32 bits, which picks the bucket, is the
//! // high half of the same hash.
//! fn hash_texts(texts: &[&str]) -> (Vec<u64>, Vec<u32>) {
//!     let words: Vec<u64> = texts
//!         .iter()
//!         .map(|text| {
//!             let mut hasher = DefaultHasher::new();
//!             text.hash(&mut hasher);
//!             hasher.finish()
//!         })
//!         .collect();
//!     let hashes = words.iter().map(|word| (word >> 32) as u32).collect();
//!     (words, hashes)
//! }
//!
//! struct Words<'a>(&'a [u64]);
//! impl KeySource for Words<'_> {
//!     fn nkeys(&self) -> usize { 1 }
//!     fn nrows(&self) -> usize { self.0.len() }
//!     fn word(&self, _: usize, index: usize, selected: u64, out: &mut [i64; 64]) -> Option<u64> {
//!         for bit in ones(selected) {
//!             out[bit] = self.0[index * 64 + bit] as i64;
//!         }
//!         Some(selected)
//!     }
//! }
//!
//! // The inner side. The table keeps no string, so each record's payload
//! // says where its string lies: here its index in `inner`.
//! let inner = ["apple", "pear", "plum"];
//! let (words, hashes) = hash_texts(&inner);
//! let payload: Vec<u8> = (0..inner.len() as u64).flat_map(u64::to_ne_bytes).collect();
//! let config = TableConfig { nkeys: 1, payload_size: 8 };
//! let mut table = OwnedTable::new(&config, 100, 4096)?;
//! let all = RowMaskView::try_new(3, &[0b111])?;
//! table.insert(&hashes, &Words(&words), all, Some(&payload), &mut [0; 3])?;
//!
//! // The outer side. The table finds the records whose 64-bit hash is the
//! // row's own. Two strings may share a hash, so the row compares the
//! // strings and keeps the equal ones.
//! let outer = ["pear", "fig"];
//! let (words, hashes) = hash_texts(&outer);
//! let table = table.table()?;
//! let (mut rows, mut refs) = ([0b11], [0; 2]);
//! let mut matches = Matches::new(&mut rows, &mut refs)?;
//! let mut found = [None; 2];
//! // `left` counts the rows that have a record to look at; a row whose
//! // hash no record has leaves the mask.
//! let mut left = table.probe(&hashes, &Words(&words), &mut matches)?;
//! while left > 0 {
//!     for row in matches.rows().selected_rows() {
//!         let payload = table.record(matches.refs()[row])?.payload;
//!         let index = u64::from_ne_bytes(payload.try_into().unwrap()) as usize;
//!         if inner[index] == outer[row] {
//!             found[row] = Some(index);
//!         }
//!     }
//!     // Each row goes to its next record of the same hash, if it has one.
//!     left = table.next_match(&mut matches)?;
//! }
//! assert_eq!(found, [Some(1), None]);
//! # }
//! # Ok::<(), relhash::Error>(())
//! ```

#![deny(unsafe_code)]
#![deny(missing_docs)]
// Without `alloc` the owning table is not built, and the documentation's
// links to it have nothing to point to.
#![cfg_attr(not(feature = "alloc"), allow(rustdoc::broken_intra_doc_links))]

// The arithmetic of offsets assumes a usize of 64 bits: the bytes of 2^31
// buckets would wrap on a narrower one and pass the check of an index.
#[cfg(not(target_pointer_width = "64"))]
compile_error!("relhash needs a target whose pointers are 64 bits wide");

mod bits;
mod error;
mod row_mask;
mod table;

pub use bits::ones;
pub use error::{Error, Kind, Rule};
pub(crate) use row_mask::check_rows;
pub use row_mask::{RowMask, RowMaskView};
pub use table::*;
