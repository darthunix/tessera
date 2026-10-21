//! A hash table of records that never move, for the joins and the
//! groupings of relational databases.
//!
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
//! - The **index** is a header of fixed sizes (`header.rs`) and an array of
//!   buckets. A row's hash picks its bucket by its high bits.
//! - A **chunk** starts with its **used mark**, the bytes in use, and
//!   holds records one after another.
//! - A **record** is one row: its hash, the reference of the next record
//!   of its chain, the NULL bits of its keys, a word of 8 bytes per key,
//!   and a **payload**, bytes the table never reads. A join keeps the row
//!   it returns there, a grouping the states of its aggregates.
//! - A **reference** names a record: its chunk and its place in the
//!   chunk, in 32 bits. A bucket holds the reference of the first record
//!   of its **chain**, and each record that of the next one. 0 means
//!   none, since no record starts where a chunk's used mark lies.
//!
//! A key is a word compared bit for bit: the table gives it no type. The
//! caller computes the hash, so its hash decides which keys meet: the
//! kernels of Tessera hash as PostgreSQL does.
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
//! # What a call checks
//!
//! The table trusts no byte it reads. A call checks the header, every
//! chunk before it writes or walks it, and every reference before it
//! follows it, and it walks a chain no further than the records the
//! chunks can hold. Bytes that are not a table give an error, never a
//! crash or an endless walk; the spec of `hash-table` lists the rules in
//! its requirement "Errors of a call". An error is an [`Error`]: the
//! [`Rule`] broken, its [`Kind`], a wrong call or a damaged table, and up
//! to two numbers, in one word that allocates nothing. Wrong arguments
//! fail before the call changes anything. A full chunk or index is not an
//! error: the rows without room stay in the mask, and the caller adds a
//! chunk or a larger index and calls again.
//!
//! This module may use `unsafe` for its blocks of raw memory, which it
//! reaches only through `region.rs`. [`OwnedTable`] owns its blocks and
//! needs no `unsafe`: the tests and benchmarks build their tables with it.
//!
//! ```
//! use relhash::{KeySource, Matches, OwnedTable, RowMaskView, TableConfig, ones};
//!
//! // The caller hands keys over a word of 64 rows at a time: here one key
//! // of int4 values with no NULL.
//! struct Keys<'a>(&'a [i32]);
//! impl KeySource for Keys<'_> {
//!     fn nkeys(&self) -> usize { 1 }
//!     fn nrows(&self) -> usize { self.0.len() }
//!     fn word(&self, _: usize, index: usize, selected: u64, out: &mut [i64; 64]) -> Option<u64> {
//!         for bit in ones(selected) {
//!             out[bit] = i64::from(self.0[index * 64 + bit]);
//!         }
//!         Some(selected) // the bits of the rows that are not NULL
//!     }
//! }
//!
//! let config = TableConfig { nkeys: 1, payload_size: 8 };
//! let mut table = OwnedTable::new(&config, 100, 4096)?;
//! assert_eq!(table.table()?.stats().records, 0);
//!
//! // Three rows with keys 7, 8 and 7, and their hashes: the caller's hash,
//! // the same for the same key.
//! let keys = Keys(&[7, 8, 7]);
//! let hashes: Vec<u32> = [7u32, 8, 7].map(|key| key.wrapping_mul(0x9e37_79b1)).into();
//! let payload = [1u64, 2, 3].map(u64::to_ne_bytes).concat();
//! let all = RowMaskView::try_new(3, &[1 << 0 | 1 << 1 | 1 << 2])?;
//! let mut refs = [0; 3];
//! assert_eq!(table.insert(&hashes, &keys, all, Some(&payload), &mut refs)?, 3);
//! assert!(refs.iter().all(|&reference| reference != 0), "each row has its record");
//!
//! // Look up rows 0 and 1 of the three.
//! let table = table.table()?;
//! let mut rows = [1 << 0 | 1 << 1];
//! let mut refs = [0; 3];
//! let mut matches = Matches::new(&mut rows, &mut refs)?;
//! // Both find a record: the key of row 0 has two, that of row 1 one.
//! assert_eq!(table.probe(&hashes, &keys, &mut matches)?, 2);
//! assert_eq!(table.record(matches.refs()[0])?.payload, 3u64.to_ne_bytes());
//! // Only row 0 has a next record of its key.
//! assert_eq!(table.next_match(&mut matches)?, 1);
//! assert_eq!(matches.rows().selected_rows().collect::<Vec<_>>(), [0]);
//! assert_eq!(table.record(matches.refs()[0])?.payload, 1u64.to_ne_bytes());
//! # Ok::<(), relhash::Error>(())
//! ```

#![deny(unsafe_code)]

mod bits;
mod error;
mod row_mask;
mod table;

pub use bits::ones;
pub use error::{Error, Kind, Rule};
pub use row_mask::{RowMask, RowMaskView, check_rows};
pub use table::*;
