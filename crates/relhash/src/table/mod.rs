//! A hash table of records that never move, for joins and grouping.
//!
//! A table is an index and chunks of records, blocks of memory the caller
//! owns and hands to every call: the local memory of a serial plan, or
//! dynamic shared memory of a parallel one, mapped by every process at an
//! address of its own. The table keeps no address between calls, and its
//! memory is all the caller's. The index is a header of 96 bytes (see [`header`])
//! and the bucket array, a power of two of buckets, at least 1024 and at
//! least twice the records the index was made for, each holding the
//! reference of the first record of its chain. The records lie in chunks
//! of at most [`MAX_CHUNK_LEN`] bytes, each starting with a used mark, one
//! after another: a hash, the reference of the next record of its bucket,
//! a bit per key that is NULL and its length in 8-byte units, then one
//! 8-byte slot per key (an int4 sign-extended) and the payload the table
//! was created for, rounded up to 8: opaque bytes, a join's build row or
//! grouping's aggregate states. A reference is 32 bits: the chunk's number
//! and the record's offset there in 8-byte units ([`UNIT_BITS`] of them);
//! 0 means none, since a chunk's used mark lies there. The caller passes
//! the chunks as their bases and lengths in its own process ([`Chunks`]).
//!
//! A record is appended to a chunk by the chunk's one writer
//! ([`Table::append`]), then linked into its bucket ([`Table::link`], by
//! any number of participants at once, each over chunks of its own, or
//! [`TableMut::link_grouped`], by one writer, next to the records of the
//! same keys, whose rounds then step with [`Table::next_in_group`]). A
//! record never moves: a larger index is built over the same chunks
//! ([`TableMut::regrow`]). Probing ([`Table::probe`] for the first record
//! with a row's hash and keys, [`Table::next_match`] for the ones after
//! it, [`Table::gather`] for a payload word of each match) runs once the
//! records are linked. [`TableMut`] is the access of one writer, which
//! alone may give rows the record of their keys, creating it when there is
//! none ([`TableMut::find_or_insert`], for grouping), change a payload in
//! place, or walk the records ([`Table::scan`]). The caller hashes the
//! keys, and its hash decides what NULL keys do: Tessera's kernels hash as
//! PostgreSQL does.
//!
//! Every call checks a chunk it writes or walks when it starts on it (the
//! other chunks are the caller's promise, as the validity of their memory
//! is, and a debug build of the entry points checks them all), and a call
//! that reads the index attaches anew and checks the whole header; every
//! reference is checked against its chunk before it is followed, and a
//! chain is walked at most as many steps as there are records, and never
//! more than the places a reference can name in the chunks, so a corrupt
//! table is an error, never a hang or an access past a block. Dimension
//! and pointer errors come before any change. A full chunk or index is not
//! an error: rows without room stay in their mask for the caller to retry
//! after adding a chunk or building a larger index.
//!
//! This is the second module of the crate allowed `unsafe`, for the index
//! and chunks over raw pointers and atomics on them; see [`region`].
//! [`LocalTable`] owns its index and chunks and needs no `unsafe`: tests
//! and benchmarks build tables with it.
//!
//! ```
//! use anyhow::Result;
//! use relhash::{RowMask, RowMaskView};
//! use relhash::table::{Batch, KeyKind, KeySource, LocalTable, TableConfig};
//!
//! // The caller hands keys over a word of 64 rows at a time: here one key
//! // of int4 values with no NULL.
//! struct Keys<'a>(&'a [i32]);
//! impl KeySource for Keys<'_> {
//!     fn nkeys(&self) -> usize { 1 }
//!     fn nrows(&self) -> usize { self.0.len() }
//!     fn word(&self, _: usize, index: usize, selected: u64, out: &mut [i64; 64]) -> Result<u64> {
//!         for bit in (0..64).filter(|bit| selected >> bit & 1 == 1) {
//!             out[bit] = i64::from(self.0[index * 64 + bit]);
//!         }
//!         Ok(selected) // the bits of the rows that are not NULL
//!     }
//! }
//!
//! let config = TableConfig { keys: &[KeyKind::Int32], payload_size: 8 };
//! let mut table = LocalTable::new(&config, 100, 4096)?;
//! assert_eq!(table.table()?.stats().records, 0);
//!
//! // Three rows with keys 7, 8 and 7, and their hashes: the caller's hash,
//! // the same for the same key.
//! let keys = Keys(&[7, 8, 7]);
//! let hashes: Vec<u32> = [7u32, 8, 7].map(|key| key.wrapping_mul(0x9e37_79b1)).into();
//! let payload = [1u64, 2, 3].map(u64::to_ne_bytes).concat();
//! let mut pending = [0b111];
//! let mut offsets = [0; 3];
//! let mut mask = RowMask::try_new(3, &mut pending)?;
//! let mut batch = Batch::new(&hashes, &keys, &mut mask, &mut offsets)?;
//! table.insert(Some(&payload), &mut batch)?;
//! assert_eq!(pending, [0], "every row found room");
//!
//! // The first row's key has two records; the second row's has one.
//! let table = table.table()?;
//! let mut hits = [0];
//! let mut matches = [0; 3];
//! let mut found = RowMask::try_new(3, &mut hits)?;
//! table.probe(&hashes, &keys, &RowMaskView::try_new(3, &[0b011])?, &mut matches, &mut found)?;
//! assert_eq!(hits, [0b011]);
//! assert_eq!(table.record(matches[0])?.payload, 3u64.to_ne_bytes());
//! let mut more = RowMask::try_new(3, &mut hits)?;
//! table.next_match(&mut matches, &RowMaskView::try_new(3, &[0b011])?, &mut more)?;
//! assert_eq!(hits, [0b001]);
//! assert_eq!(table.record(matches[0])?.payload, 1u64.to_ne_bytes());
//! # Ok::<(), anyhow::Error>(())
//! ```
#![allow(unsafe_code)]

mod batch;
pub mod bloom;
mod chunks;
mod header;
mod keys;
mod lanes;
#[cfg(loom)]
pub mod loom;
mod marks;
#[cfg(feature = "alloc")]
mod owned;
mod record;
mod region;
#[cfg(any(test, loom))]
mod testing;
#[cfg(test)]
mod tests;
mod writer;

use core::marker::PhantomData;
use core::ops::Deref;

use crate::{RowMask, RowMaskView};
use anyhow::{Result, anyhow, ensure};

pub use batch::Batch;
pub use chunks::{
    Appended, Chunks, MAX_PARTITIONS, Partitions, Split, append_columns_to,
    append_partitioned_columns_to, append_to, chunk_used, init_chunk, split_to,
};
pub use header::{
    CHUNK_HEADER, FORMAT_VERSION, HEADER_SIZE, KeyKind, MAX_CHUNK_LEN, MAX_CHUNKS, MAX_KEYS,
    TableConfig, UNIT_BITS, VERSION_OFFSET, index_size, record_bytes, record_bytes_of,
};
use header::{Header, Layout, NRECORDS};
pub use keys::KeySource;
pub use marks::{Marks, mark, mark_words, scan_unmarked};
#[cfg(feature = "alloc")]
pub use owned::LocalTable;
use record::Access;
pub use record::{MAX_PAYLOAD_COLUMNS, PayloadColumns, Record, payload_null_words};
use region::{RawRegion, Region};
pub use writer::{CombineStop, Cursor, HELD, PayloadCursor};

/// What a table holds, for planning and EXPLAIN.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Stats {
    /// Records linked into the buckets.
    pub records: u64,
    /// Buckets of the index.
    pub buckets: u64,
    /// Bytes of the index in use: the header and the buckets.
    pub bytes_used: u64,
    /// Bytes of the index the table was created or regrown over.
    pub region_len: u64,
}

/// The error of a call whose keys or payload size are not its table's.
#[cold]
#[inline(never)]
fn other_config(config: &TableConfig<'_>, layout: &Layout) -> anyhow::Error {
    anyhow!(
        "records of keys {:?} and a payload of {} bytes are not the table's: keys {:?} and {} bytes",
        config.keys,
        config.payload_size,
        &layout.kinds[..layout.nkeys],
        layout.payload_size
    )
}

/// A table over an index and chunks, as several participants may share it.
///
/// It is neither `Send` nor `Sync`: each participant attaches its own.
#[derive(Debug)]
pub struct Table<'a> {
    region: RawRegion,
    layout: Layout,
    _region: PhantomData<&'a ()>,
}

/// A table that one writer has to itself.
#[derive(Debug)]
pub struct TableMut<'a>(Table<'a>);

impl<'a> Deref for TableMut<'a> {
    type Target = Table<'a>;

    fn deref(&self) -> &Table<'a> {
        &self.0
    }
}

/// Write the header of a new table's index for `capacity` records and
/// clear its buckets; the layout is returned.
///
/// # Safety
///
/// The caller has the index to itself, so nothing else reads or writes it
/// meanwhile.
unsafe fn init<R: Region>(region: &R, config: &TableConfig<'_>, capacity: u64) -> Result<Layout> {
    let header = Header::new(config, capacity, region.len())?;
    let layout = header.validate(region.len())?;
    header.store(region);
    // SAFETY: the caller's contract.
    unsafe { region.zero_u32(layout.buckets_offset, layout.nbuckets as usize) };
    Ok(layout)
}

/// Reject an index that could not hold a header.
fn check_region(region: *mut u8, len: usize) -> Result<()> {
    ensure!(
        !region.is_null() && region.addr().is_multiple_of(8),
        "a table index must be aligned to 8 bytes"
    );
    ensure!(
        len >= HEADER_SIZE,
        "a table index of {len} bytes is shorter than the {HEADER_SIZE}-byte header"
    );
    Ok(())
}

impl<'a> Table<'a> {
    /// Attach to the table whose index is the `len` bytes at `index`, over
    /// `chunks`, checking its header.
    ///
    /// # Safety
    ///
    /// `index` is aligned to 8 and valid for reads and writes of `len`
    /// bytes for `'a`, and during `'a` the index and the chunks are
    /// accessed only through tables, here or in other processes mapping
    /// the same memory; probes run only once the records they may find
    /// are linked, and a chunk has one writer.
    pub unsafe fn attach(index: *mut u8, len: usize, chunks: Chunks<'a>) -> Result<Self> {
        check_region(index, len)?;
        // SAFETY: the caller's contract, and `Chunks::new`'s for the chunks.
        let region = unsafe {
            RawRegion::new(
                index,
                len,
                chunks.bases.as_ptr(),
                chunks.lens.as_ptr(),
                chunks.len(),
            )
        };
        let layout = Header::load(&region).validate(len)?;
        Ok(Self {
            region,
            layout,
            _region: PhantomData,
        })
    }

    /// The kinds of the keys, in key order. Inlined into the callers of
    /// other crates, as it was when they lay in one crate.
    #[inline]
    pub fn key_kinds(&self) -> &[KeyKind] {
        &self.layout.kinds[..self.layout.nkeys]
    }

    /// Bytes of payload per record.
    pub fn payload_size(&self) -> usize {
        self.layout.payload_size
    }

    /// Bytes of one record.
    pub fn record_size(&self) -> usize {
        self.layout.record_size
    }

    /// The fingerprint of the table's record layout, which spilled blocks
    /// carry so that they are read back only into a table like it.
    pub fn fingerprint(&self) -> u64 {
        self.layout.fingerprint()
    }

    /// Refuse records of `config` unless its keys and payload size are the
    /// table's: a call that is given the layout of the records it writes,
    /// as an append is, checks it against the index when there is one, so
    /// that a caller's mistake is not written and found later as damage.
    pub fn check_config(&self, config: &TableConfig<'_>) -> Result<()> {
        if self.key_kinds() == config.keys && self.layout.payload_size == config.payload_size {
            Ok(())
        } else {
            Err(other_config(config, &self.layout))
        }
    }

    /// The counts of the table as of now.
    pub fn stats(&self) -> Stats {
        let buckets = u64::from(self.layout.nbuckets);
        Stats {
            records: self.region.load_u64(NRECORDS),
            buckets,
            bytes_used: (HEADER_SIZE as u64) + buckets * 4,
            region_len: self.layout.region_len as u64,
        }
    }

    /// Append the pending rows of `batch` as records to chunk `chunk`, in
    /// row order, as long as whole records fit: each row appended leaves
    /// the batch's pending rows and gets the reference of its record; the
    /// count appended is returned, and rows still pending need another
    /// chunk. The batch's keys are the table's keys; `payload` is the
    /// payload of every physical row one after another, or `None` for
    /// zeros. The records are not in the buckets until linked. The caller
    /// must be the chunk's one writer.
    pub fn append<K: KeySource + ?Sized>(
        &self,
        chunk: usize,
        payload: Option<&[u8]>,
        batch: &mut Batch<'_, '_, K>,
    ) -> Result<usize> {
        batch::append(&self.region, &self.layout, chunk, payload, batch)
    }

    /// As [`Table::append`], with each row's payload taken from `columns`:
    /// a word of its NULL bits, then a word per column, which must be the
    /// table's whole payload.
    pub fn append_columns<K: KeySource + ?Sized>(
        &self,
        chunk: usize,
        columns: &PayloadColumns<'_>,
        batch: &mut Batch<'_, '_, K>,
    ) -> Result<usize> {
        batch::append_columns(&self.region, &self.layout, chunk, columns, batch)
    }

    /// Link the records of chunk `chunk` from byte `*from` to its used
    /// mark into their buckets, first in their chains, and move `*from`
    /// past them; the count linked is returned. Several participants may
    /// link at once, each its own chunks; equal keys make separate
    /// records. `*from` starts at [`CHUNK_HEADER`].
    pub fn link(&self, chunk: usize, from: &mut usize) -> Result<usize> {
        batch::link(&self.region, &self.layout, chunk, from)
    }

    /// As [`Self::link`], also counting the records whose keys the table
    /// held already, each once, even while other participants link: a
    /// published record walks the rest of its chain for its keys. Returns
    /// the count linked and the duplicates.
    pub fn link_counting(&self, chunk: usize, from: &mut usize) -> Result<(usize, usize)> {
        batch::link_counting::<_, true>(&self.region, &self.layout, chunk, from)
    }

    /// Find the first record of its chain with the hash, null bits and
    /// keys of each row of `rows`: `matches[row]` receives its reference
    /// and `found` the rows that have one, as a mask this call produces.
    /// The other records with the same keys follow through
    /// [`Table::next_match`], or [`Table::next_in_group`] in a table linked
    /// by [`TableMut::link_grouped`].
    pub fn probe<K: KeySource + ?Sized>(
        &self,
        hashes: &[u32],
        keys: &K,
        rows: &RowMaskView<'_>,
        matches: &mut [u32],
        found: &mut RowMask<'_>,
    ) -> Result<()> {
        batch::probe(
            &self.region,
            &self.layout,
            hashes,
            keys,
            rows,
            matches,
            found,
        )
    }

    /// For each row of `rows`, replace `offsets[row]`, a record reference
    /// from a probe or an earlier call, by the reference of the next record
    /// in its chain with the same hash, null bits and keys; `found`
    /// receives the rows that have one, and the others keep theirs.
    pub fn next_match(
        &self,
        offsets: &mut [u32],
        rows: &RowMaskView<'_>,
        found: &mut RowMask<'_>,
    ) -> Result<()> {
        batch::next_match(&self.region, &self.layout, offsets, rows, found)
    }

    /// For each row of `rows`, the word at byte `at` of the payload of the
    /// record at `offsets[row]` into `out[row]`, native-endian: one word of
    /// a batch's matches per call, such as a Datum of the build row a join
    /// keeps there. `at` must be a multiple of 8 and `at + 8` within the
    /// payload; rows outside `rows` keep their values in `out`.
    pub fn gather(
        &self,
        offsets: &[u32],
        rows: &RowMaskView<'_>,
        at: usize,
        out: &mut [u64],
    ) -> Result<()> {
        batch::gather::<_, false>(&self.region, &self.layout, offsets, rows, at, out)
    }

    /// As [`Table::gather`], for records in no order, as a sort reads
    /// them back: those of a word of rows are prefetched before any is
    /// read. Records just probed are in the cache, where [`Table::gather`]
    /// is faster.
    pub fn gather_scattered(
        &self,
        offsets: &[u32],
        rows: &RowMaskView<'_>,
        at: usize,
        out: &mut [u64],
    ) -> Result<()> {
        batch::gather::<_, true>(&self.region, &self.layout, offsets, rows, at, out)
    }

    /// For each row of `rows`, payload word `first + n` (in 8-byte units)
    /// of the record at `offsets[row]` into `out[n][row]`, native-endian,
    /// for every `out[n]` of a row each, records in no order as
    /// [`Table::gather_scattered`] reads them: one call for every column a
    /// sort's batch serves, a record located once for all of them. Rows
    /// outside `rows` keep their values.
    pub fn gather_words(
        &self,
        offsets: &[u32],
        rows: &RowMaskView<'_>,
        first: usize,
        out: &mut [&mut [u64]],
    ) -> Result<()> {
        batch::gather_words(&self.region, &self.layout, offsets, rows, first, out)
    }

    /// For each row of `rows`, replace `offsets[row]` by the record right
    /// after it when that one has the same hash, null bits and keys, and
    /// put the row in `found`; other rows keep theirs. In a table linked by
    /// [`TableMut::link_grouped`] this is the next record of the key,
    /// found in one step instead of a walk down the chain.
    pub fn next_in_group(
        &self,
        offsets: &mut [u32],
        rows: &RowMaskView<'_>,
        found: &mut RowMask<'_>,
    ) -> Result<()> {
        batch::next_in_group(&self.region, &self.layout, offsets, rows, found)
    }

    /// For each row of `rows`, key `key` of the record at `offsets[row]`:
    /// its slot's bits into `values[row]` (an int4 sign-extended, as its
    /// Datum is, 0 for a NULL) and whether it is NULL into `nulls[row]`.
    /// Rows outside `rows` keep their values.
    pub fn gather_key(
        &self,
        offsets: &[u32],
        rows: &RowMaskView<'_>,
        key: usize,
        values: &mut [u64],
        nulls: &mut [bool],
    ) -> Result<()> {
        batch::gather_key(
            &self.region,
            &self.layout,
            offsets,
            rows,
            key,
            values,
            nulls,
        )
    }

    /// Fill a Bloom filter of [`bloom::words_for`] this table's records
    /// words (or any power of two) with the hash of every record of every
    /// chunk, after clearing it: a probe row it rejects has no record with
    /// its hash. No chunk may take records at the same time.
    pub fn bloom(&self, words: &mut [u64]) -> Result<()> {
        bloom::fill(&self.region, &self.layout, words)
    }

    /// Build a shared filter of this table's records unless another
    /// participant has claimed it: true for the one that built it. Call
    /// it, like [`Table::bloom`], while no chunk takes records.
    pub fn try_build_bloom(&self, filter: &bloom::SharedFilter<'_>) -> Result<bool> {
        bloom::try_build(&self.region, &self.layout, filter)
    }

    /// The record at a reference a call of this table returned.
    pub fn record(&self, offset: u32) -> Result<Record<'_>> {
        Ok(Access::new(&self.region, &self.layout)
            .locate(offset)?
            .record())
    }

    /// A cursor that reads the records references name, made once a call:
    /// it reads the table's header once, and checks each reference as a
    /// lookup checks it.
    #[inline(always)]
    pub fn records(&self) -> RecordCursor<'_> {
        RecordCursor {
            access: Access::new(&self.region, &self.layout),
        }
    }

    /// Visit the records from `cursor` on, chunk by chunk in the order
    /// they were appended, as many as `out` holds: their references fill
    /// `out`, the count is returned and the cursor moves past them; 0
    /// means the walk is over.
    pub fn scan(&self, cursor: &mut Cursor, out: &mut [u32]) -> Result<usize> {
        writer::scan(&self.region, &self.layout, cursor, out)
    }
}

/// A cursor that reads the records of a table by reference, made once a
/// call by [`Table::records`]: it keeps the table's access, whose fields
/// the loops hold in registers. The records it gives live as long as the
/// table's borrow.
pub struct RecordCursor<'t> {
    access: Access<'t, RawRegion>,
}

impl<'t> RecordCursor<'t> {
    /// The record at `offset`: an error for a reference that names no
    /// record of the table.
    #[inline(always)]
    pub fn record(&mut self, offset: u32) -> Result<Record<'t>> {
        Ok(self.access.locate(offset)?.record())
    }
}

impl<'a> TableMut<'a> {
    /// Create an empty table for `capacity` records with its index in the
    /// `len` bytes at `index`, which must be a multiple of 8 and at least
    /// [`index_size`], over `chunks`.
    ///
    /// # Safety
    ///
    /// As [`Table::attach`], and no other table is over the index while
    /// this one exists.
    pub unsafe fn create(
        index: *mut u8,
        len: usize,
        config: &TableConfig<'_>,
        capacity: u64,
        chunks: Chunks<'a>,
    ) -> Result<Self> {
        check_region(index, len)?;
        // SAFETY: the caller's contract.
        let region = unsafe {
            RawRegion::new(
                index,
                len,
                chunks.bases.as_ptr(),
                chunks.lens.as_ptr(),
                chunks.len(),
            )
        };
        // SAFETY: the caller has the index to itself.
        let layout = unsafe { init(&region, config, capacity) }?;
        Ok(Self(Table {
            region,
            layout,
            _region: PhantomData,
        }))
    }

    /// Attach as the one writer of a table.
    ///
    /// # Safety
    ///
    /// As [`TableMut::create`].
    pub unsafe fn attach_mut(index: *mut u8, len: usize, chunks: Chunks<'a>) -> Result<Self> {
        // SAFETY: the caller's contract.
        unsafe { Table::attach(index, len, chunks) }.map(Self)
    }

    /// Give each pending row of `batch` the record of its keys, creating
    /// one with a zero payload in chunk `chunk` where none exists, in row
    /// order, until the chunk has no room or the index holds records for
    /// half its buckets: resolved rows leave the pending rows and get their
    /// record references, the rows whose record this call created form
    /// `inserted`, and the count resolved is returned. Rows left pending
    /// need another chunk or a larger index ([`TableMut::regrow`]).
    pub fn find_or_insert<K: KeySource + ?Sized>(
        &mut self,
        chunk: usize,
        batch: &mut Batch<'_, '_, K>,
        inserted: &mut RowMask<'_>,
    ) -> Result<usize> {
        writer::find_or_insert(&self.0.region, &self.0.layout, chunk, batch, inserted)
    }

    /// Give each pending row of `batch` the record of its keys, as
    /// [`TableMut::find_or_insert`] does, but a new record goes to the chunk
    /// of its hash's partition: a row whose partition's chunk is full stays
    /// pending while the rows after it go on, and all stop once the records
    /// reach half the buckets. For a grouping that spills.
    pub fn find_or_insert_partitioned<K: KeySource + ?Sized>(
        &mut self,
        partitions: &Partitions<'_>,
        batch: &mut Batch<'_, '_, K>,
        inserted: &mut RowMask<'_>,
    ) -> Result<usize> {
        writer::find_or_insert_partitioned(
            &self.0.region,
            &self.0.layout,
            partitions,
            batch,
            inserted,
        )
    }

    /// Merge the records of chunk `source` from byte `*from` on into the
    /// table by `rule`, which gets the payload of a record of keys the
    /// table has and the payload of the table's record, to change; a
    /// record of new keys is copied whole to chunk `chunk` and linked.
    /// `*from` moves past the records merged, and the call stops where a
    /// record of new keys needs another chunk or a larger index; the
    /// count merged is returned. When the rule fails, the call returns
    /// its error, the records before it merged.
    pub fn merge(
        &mut self,
        source: usize,
        from: &mut usize,
        chunk: usize,
        rule: impl FnMut(&[u8], &mut [u8]) -> Result<()>,
    ) -> Result<(usize, CombineStop)> {
        writer::merge(&self.0.region, &self.0.layout, source, from, chunk, rule)
    }

    /// Link the records of chunk `chunk` from byte `*from` on, as
    /// [`Table::link`] does, but each right after a record with the same
    /// keys when the table holds one, so that a key's records lie next to
    /// each other and [`Table::next_in_group`] steps through them. Returns
    /// the records linked and how many of them had keys the table held
    /// already.
    pub fn link_grouped(&mut self, chunk: usize, from: &mut usize) -> Result<(usize, usize)> {
        writer::link_grouped(&self.0.region, &self.0.layout, chunk, from)
    }

    /// A cursor over the payloads of the table's records, to change them
    /// by reference, one at a time: made once a call, it reads the table's
    /// header once.
    #[inline(always)]
    pub fn payloads(&mut self) -> PayloadCursor<'_> {
        PayloadCursor::new(&mut self.0)
    }

    /// The payload of the record at a reference, to change in place.
    pub fn payload_mut(&mut self, offset: u32) -> Result<&mut [u8]> {
        writer::payload_mut(&self.0.region, &self.0.layout, offset)
    }

    /// Hand `each` the row and the payload of the record of each selected
    /// row, in the order of the rows, to change in place: the payloads of
    /// a batch, which hold the references [`TableMut::find_or_insert`]
    /// gave them, for one check of the table. `offsets` has a reference
    /// per row of the mask; a reference that names no record fails the
    /// call, after the rows before it.
    pub fn each_payload(
        &mut self,
        offsets: &[u32],
        rows: &RowMaskView<'_>,
        each: impl FnMut(usize, &mut [u8]),
    ) -> Result<()> {
        writer::each_payload(&self.0.region, &self.0.layout, offsets, rows, each)
    }

    /// Write 0 into key `key` of every record, its NULL bit kept; the count
    /// of records is returned. The key then orders no two records that are
    /// not NULL, and a probe by it no longer finds them: for records nothing
    /// looks up by their keys, such as a sort's.
    pub fn clear_key(&mut self, key: usize) -> Result<u64> {
        writer::clear_key(&self.0.region, &self.0.layout, key)
    }

    /// Move the table to a new index of `len` bytes at `index`, for
    /// `capacity` records, over the same chunks: every record is linked
    /// there again, grouped chains stay grouped, and the old index is no
    /// longer the table's. Records and their references stay as they were.
    ///
    /// # Safety
    ///
    /// As [`TableMut::create`] for the new index, which must live for
    /// `'a`.
    pub unsafe fn regrow(&mut self, index: *mut u8, len: usize, capacity: u64) -> Result<()> {
        check_region(index, len)?;
        // SAFETY: the caller's contract.
        let to = unsafe { self.0.region.with_index(index, len) };
        self.0.layout = writer::regrow(&self.0.region, &to, &self.0.layout, capacity)?;
        self.0.region = to;
        Ok(())
    }
}
