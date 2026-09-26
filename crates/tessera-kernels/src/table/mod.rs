//! A hash table of records that never move, for joins and grouping.
//!
//! A table is an index and chunks of records, blocks of memory the caller
//! owns and hands to every call: the local memory of a serial plan, or
//! dynamic shared memory of a parallel one, mapped by every process at an
//! address of its own. The table keeps no address between calls and
//! allocates nothing. The index is a header of 96 bytes (see [`header`])
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
//! place, or walk the records ([`TableMut::scan`]). Hashes come from
//! [`crate::int32::hash`], [`crate::int64::hash`] and their `hash_next`,
//! which decide what NULL keys do.
//!
//! Every call attaches anew and checks the whole header; every reference
//! is checked against its chunk before it is followed, and a chain is
//! walked at most as many steps as there are records, so a corrupt table
//! is an error, never a hang or an access past a block. Dimension errors
//! come before any change. A full chunk or index is not an error: rows
//! without room stay in their mask for the caller to retry after adding a
//! chunk or building a larger index.
//!
//! This is the second module of the crate allowed `unsafe`, for the index
//! and chunks over raw pointers and atomics on them; see [`region`].
//! [`LocalTable`] owns its index and chunks and needs no `unsafe`: tests
//! and benchmarks build tables with it.
//!
//! ```
//! use tessera_kernels::table::{KeyKind, LocalTable, TableConfig};
//!
//! let config = TableConfig { keys: &[KeyKind::Int32], payload_size: 8 };
//! let mut table = LocalTable::new(&config, 100, 4096)?;
//! assert_eq!(table.table()?.stats().records, 0);
//!
//! // Three rows with keys 7, 8 and 7, hashed by the int4 kernel's formula.
//! use tessera_core::{ColumnView, RowMask, RowMaskView};
//! use tessera_kernels::int32::murmurhash32;
//! let keys = [ColumnView::try_new(&[7, 8, 7], None)?];
//! let hashes: Vec<u32> = [7, 8, 7].map(|key: i32| murmurhash32(key as u32)).into();
//! let payload = [1u64, 2, 3].map(u64::to_ne_bytes).concat();
//! let mut pending = [0b111];
//! let mut offsets = [0; 3];
//! let mut mask = RowMask::try_new(3, &mut pending)?;
//! table.insert(&hashes, &keys[..], Some(&payload), &mut mask, &mut offsets)?;
//! assert_eq!(pending, [0], "every row found room");
//!
//! // The first row's key has two records; the second row's has one.
//! let table = table.table()?;
//! let mut hits = [0];
//! let mut matches = [0; 3];
//! let mut found = RowMask::try_new(3, &mut hits)?;
//! table.probe(&hashes, &keys[..], &RowMaskView::try_new(3, &[0b011])?, &mut matches, &mut found)?;
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
mod exclusive;
mod header;
mod keys;
mod lanes;
mod local;
#[cfg(all(test, loom))]
mod loom;
pub mod phases;
mod record;
mod region;

use core::marker::PhantomData;
use core::ops::Deref;

use anyhow::{Result, ensure};
use tessera_core::{ColumnReader, RowMask, RowMaskView};

pub use exclusive::{Cursor, Fold, Slot};
pub use header::{
    CHUNK_HEADER, FORMAT_VERSION, HEADER_SIZE, KeyKind, MAX_CHUNK_LEN, MAX_CHUNKS, MAX_KEYS,
    TableConfig, UNIT_BITS, VERSION_OFFSET, index_size, record_bytes,
};
use header::{Header, Layout, NRECORDS};
pub use keys::{KeySource, KeyValue, normalize_word};
pub use local::LocalTable;
use record::Access;
pub use record::Record;
use region::{RawRegion, Region};

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

/// The chunks of a table as this process sees them: each one's base and
/// length.
#[derive(Clone, Copy, Debug)]
pub struct Chunks<'a> {
    bases: &'a [*mut u8],
    lens: &'a [usize],
}

impl<'a> Chunks<'a> {
    /// No chunk.
    pub fn none() -> Self {
        Self {
            bases: &[],
            lens: &[],
        }
    }

    /// The chunks at `bases`, of the lengths in `lens`, after checking that
    /// the arrays match, there are at most [`MAX_CHUNKS`], and every chunk
    /// is aligned to 8, a multiple of 8 of [`CHUNK_HEADER`] to
    /// [`MAX_CHUNK_LEN`] bytes.
    ///
    /// # Safety
    ///
    /// Every base is valid for reads and writes of its length for `'a`,
    /// and during `'a` the chunks are accessed only through tables, here or
    /// in other processes mapping the same memory.
    pub unsafe fn new(bases: &'a [*mut u8], lens: &'a [usize]) -> Result<Self> {
        ensure!(
            bases.len() == lens.len() && bases.len() <= MAX_CHUNKS,
            "a table has at most {MAX_CHUNKS} chunks with a length each, not {} bases and {} lengths",
            bases.len(),
            lens.len()
        );
        for (chunk, (&base, &len)) in bases.iter().zip(lens).enumerate() {
            ensure!(
                !base.is_null()
                    && base.addr().is_multiple_of(8)
                    && len.is_multiple_of(8)
                    && (CHUNK_HEADER..=MAX_CHUNK_LEN).contains(&len),
                "table chunk {chunk} of {len} bytes is not aligned to 8 or not a multiple of 8 \
                 of {CHUNK_HEADER} to {MAX_CHUNK_LEN} bytes"
            );
        }
        Ok(Self { bases, lens })
    }

    /// The number of chunks.
    pub fn len(&self) -> usize {
        self.bases.len()
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.bases.is_empty()
    }
}

/// Make a block of `len` bytes at `base` an empty chunk: its used mark
/// covers only itself.
///
/// # Safety
///
/// `base` is aligned to 8 and valid for writes of `len` bytes, a multiple
/// of 8 of [`CHUNK_HEADER`] to [`MAX_CHUNK_LEN`], and nothing else uses the
/// block yet.
pub unsafe fn init_chunk(base: *mut u8, len: usize) -> Result<()> {
    ensure!(
        !base.is_null()
            && base.addr().is_multiple_of(8)
            && len.is_multiple_of(8)
            && (CHUNK_HEADER..=MAX_CHUNK_LEN).contains(&len),
        "a table chunk of {len} bytes is not aligned to 8 or not a multiple of 8 of \
         {CHUNK_HEADER} to {MAX_CHUNK_LEN} bytes"
    );
    // SAFETY: the caller's contract; the used mark is the first word.
    unsafe { base.cast::<u64>().write(CHUNK_HEADER as u64) };
    Ok(())
}

/// Append the rows of `pending` as records of a table of `config` to chunk
/// `chunk` of `chunks`, as [`Table::append`] does, before the table has an
/// index: the participants of a shared build append their share first,
/// and one of them sizes the index for the records once all are counted.
/// The caller must be the chunk's one writer.
#[allow(clippy::too_many_arguments)]
pub fn append_to<K: KeySource + ?Sized>(
    config: &TableConfig<'_>,
    chunks: Chunks<'_>,
    chunk: usize,
    hashes: &[u32],
    keys: &K,
    payload: Option<&[u8]>,
    pending: &mut RowMask<'_>,
    offsets: &mut [u32],
) -> Result<usize> {
    let layout = header::chunk_layout(config)?;
    // SAFETY: an empty index, which appending never reads, and the chunks
    // `Chunks::new` accepted.
    let region = unsafe {
        RawRegion::new(
            core::ptr::NonNull::<u64>::dangling().as_ptr().cast(),
            0,
            chunks.bases.as_ptr(),
            chunks.lens.as_ptr(),
            chunks.len(),
        )
    };
    batch::append(
        &region, &layout, chunk, hashes, keys, payload, pending, offsets,
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

    /// The kinds of the keys, in key order.
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

    /// Append the rows of `pending` as records to chunk `chunk`, in row
    /// order, as long as whole records fit: each row appended leaves
    /// `pending` and gets the reference of its record in `offsets`; the
    /// count appended is returned, and rows still pending need another
    /// chunk. `hashes` has one hash per physical row, `keys` the table's
    /// keys, `payload` the payload of every physical row one after another
    /// or `None` for zeros. The records are not in the buckets until
    /// linked. The caller must be the chunk's one writer.
    #[allow(clippy::too_many_arguments)]
    pub fn append<K: KeySource + ?Sized>(
        &self,
        chunk: usize,
        hashes: &[u32],
        keys: &K,
        payload: Option<&[u8]>,
        pending: &mut RowMask<'_>,
        offsets: &mut [u32],
    ) -> Result<usize> {
        batch::append(
            &self.region,
            &self.layout,
            chunk,
            hashes,
            keys,
            payload,
            pending,
            offsets,
        )
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

    /// For each row of `rows`, the 8 bytes at byte `at` of the payload of
    /// the record at `offsets[row]` into `out[row]`, native-endian: one
    /// word of a batch's matches per call, such as a Datum of the build
    /// row a join keeps there. `at + 8` must be within the payload; rows
    /// outside `rows` keep their values in `out`.
    pub fn gather(
        &self,
        offsets: &[u32],
        rows: &RowMaskView<'_>,
        at: usize,
        out: &mut [u64],
    ) -> Result<()> {
        batch::gather(&self.region, &self.layout, offsets, rows, at, out)
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

    /// Give each row of `pending` the record of its keys, creating one
    /// with a zero payload in chunk `chunk` where none exists, in row
    /// order, until the chunk has no room or the index holds records for
    /// half its buckets: resolved rows leave `pending` and get their record
    /// references in `offsets`, the rows whose record this call created
    /// form `inserted`, and the count resolved is returned. Rows left
    /// pending need another chunk or a larger index ([`TableMut::regrow`]).
    pub fn find_or_insert<K: KeySource + ?Sized>(
        &mut self,
        chunk: usize,
        hashes: &[u32],
        keys: &K,
        pending: &mut RowMask<'_>,
        offsets: &mut [u32],
        inserted: &mut RowMask<'_>,
    ) -> Result<usize> {
        exclusive::find_or_insert(
            &self.0.region,
            &self.0.layout,
            chunk,
            hashes,
            keys,
            pending,
            offsets,
            inserted,
        )
    }

    /// Link the records of chunk `chunk` from byte `*from` on, as
    /// [`Table::link`] does, but each right after a record with the same
    /// keys when the table holds one, so that a key's records lie next to
    /// each other and [`Table::next_in_group`] steps through them. Returns
    /// the records linked and how many of them had keys the table held
    /// already.
    pub fn link_grouped(&mut self, chunk: usize, from: &mut usize) -> Result<(usize, usize)> {
        exclusive::link_grouped(&self.0.region, &self.0.layout, chunk, from)
    }

    /// The payload of the record at a reference, to change in place.
    pub fn payload_mut(&mut self, offset: u32) -> Result<&mut [u8]> {
        exclusive::payload_mut(&self.0.region, &self.0.layout, offset)
    }

    /// Add one to the `i64` at byte `at` of the payload of each selected
    /// row's record: `count(*)` of a grouped aggregate, whose rows hold
    /// the references [`TableMut::find_or_insert`] gave them.
    pub fn count_rows(&mut self, offsets: &[u32], rows: &RowMaskView<'_>, at: usize) -> Result<()> {
        exclusive::count_rows(&self.0.region, &self.0.layout, offsets, rows, at)
    }

    /// Add one to the `i64` at byte `at` for each selected row whose
    /// value in `column` is not NULL: `count(x)`.
    pub fn count_values<C: ColumnReader + ?Sized>(
        &mut self,
        offsets: &[u32],
        rows: &RowMaskView<'_>,
        column: &C,
        at: usize,
    ) -> Result<()> {
        exclusive::count_values(&self.0.region, &self.0.layout, offsets, rows, column, at)
    }

    /// Fold each selected row's non-NULL value into the aggregate state
    /// at `slot` of its record's payload, in row order; the state's flag
    /// marks that it has a value. A sum that overflows an `i64` fails
    /// with [`crate::ops::ArithmeticError::BigintOutOfRange`].
    pub fn fold<C, V>(
        &mut self,
        offsets: &[u32],
        rows: &RowMaskView<'_>,
        column: &C,
        fold: Fold,
        slot: Slot,
    ) -> Result<()>
    where
        C: ColumnReader<Value = V> + ?Sized,
        V: Into<i64> + Copy,
    {
        exclusive::fold(
            &self.0.region,
            &self.0.layout,
            offsets,
            rows,
            column,
            fold,
            slot,
        )
    }

    /// Visit the records from `cursor` on, chunk by chunk in the order
    /// they were appended, as many as `out` holds: their references fill
    /// `out`, the count is returned and the cursor moves past them; 0
    /// means the walk is over.
    pub fn scan(&self, cursor: &mut Cursor, out: &mut [u32]) -> Result<usize> {
        exclusive::scan(&self.0.region, &self.0.layout, cursor, out)
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
        self.0.layout = exclusive::regrow(&self.0.region, &to, &self.0.layout, capacity)?;
        self.0.region = to;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::int32::murmurhash32;
    use tessera_core::ColumnView;

    /// The blocks of a table, which the threads of a test attach to.
    struct Shared {
        index: *mut u8,
        len: usize,
        bases: Vec<*mut u8>,
        lens: Vec<usize>,
    }
    // SAFETY: every thread accesses the blocks only through tables.
    unsafe impl Sync for Shared {}

    impl Shared {
        fn table(&self) -> Table<'_> {
            // SAFETY: the blocks outlive the borrow and are accessed only
            // through tables; the chunks are all linked before any thread
            // probes.
            unsafe {
                Table::attach(
                    self.index,
                    self.len,
                    Chunks::new(&self.bases, &self.lens).unwrap(),
                )
            }
            .unwrap()
        }
    }

    #[test]
    fn one_of_four_threads_builds_a_shared_filter_every_key_passes() {
        let config = TableConfig {
            keys: &[KeyKind::Int32],
            payload_size: 0,
        };
        let count = 200;
        let mut table = LocalTable::new(&config, count, 4096).unwrap();
        let keys: Vec<i32> = (0..count as i32).map(|key| key * 7).collect();
        let hashes: Vec<u32> = keys.iter().map(|&key| murmurhash32(key as u32)).collect();
        let column = [ColumnView::try_new(&keys, None).unwrap()];
        let mut all_rows = vec![u64::MAX; 4];
        all_rows[3] = (1 << (count - 192)) - 1;
        let mut pending_words = all_rows.clone();
        let mut pending = RowMask::try_new(count as usize, &mut pending_words).unwrap();
        let mut offsets = vec![0; count as usize];
        let inserted = table
            .insert(&hashes, &column[..], None, &mut pending, &mut offsets)
            .unwrap();
        assert_eq!(inserted, count as usize);
        assert!(table.chunks() > 1, "the records span chunks");
        // Each block's pointer and length from one borrow, the last one
        // taken of it: a later borrow would retire the pointer.
        let (index, len) = {
            let words = table.index_words();
            (words.as_mut_ptr().cast(), words.len() * 8)
        };
        let (bases, lens) = (0..table.chunks())
            .map(|chunk| {
                let words = table.chunk_words(chunk);
                (words.as_mut_ptr().cast::<u8>(), words.len() * 8)
            })
            .unzip();
        let shared = Shared {
            index,
            len,
            bases,
            lens,
        };
        let mut words = vec![0; bloom::shared_words_for(count).unwrap()];
        let filter = bloom::SharedFilter::from_mut(&mut words).unwrap();
        filter.init();
        let rows = RowMaskView::try_new(count as usize, &all_rows).unwrap();
        let mut found_words = [0; 4];
        let mut found = RowMask::try_new(count as usize, &mut found_words).unwrap();
        assert!(bloom::probe_shared(&filter, &hashes, &rows, &mut found).is_err());
        let built: usize = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..4)
                .map(|_| {
                    scope.spawn(|| {
                        let table = shared.table();
                        let built = table.try_build_bloom(&filter).unwrap();
                        while !filter.ready() {
                            std::thread::yield_now();
                        }
                        let mut found_words = [0; 4];
                        let mut found = RowMask::try_new(count as usize, &mut found_words).unwrap();
                        bloom::probe_shared(&filter, &hashes, &rows, &mut found).unwrap();
                        assert_eq!(found_words, all_rows.as_slice(), "a key was rejected");
                        usize::from(built)
                    })
                })
                .collect();
            threads
                .into_iter()
                .map(|thread| thread.join().unwrap())
                .sum()
        });
        assert_eq!(built, 1);
    }

    #[test]
    fn misaligned_or_odd_blocks_are_refused() {
        let config = TableConfig {
            keys: &[KeyKind::Int32],
            payload_size: 0,
        };
        let mut words = vec![0_u64; index_size(&config, 1000).unwrap().div_ceil(8) + 2];
        let len = words.len() * 8 - 16;
        let base = words.as_mut_ptr().cast::<u8>();
        // SAFETY: `base + 4` and `base + 8` with `len` bytes lie inside the
        // vector, which nothing else uses meanwhile.
        unsafe {
            let misaligned = base.add(4);
            assert!(TableMut::create(misaligned, len, &config, 1000, Chunks::none()).is_err());
            assert!(Table::attach(misaligned, len, Chunks::none()).is_err());
            let odd = TableMut::create(base, len - 4, &config, 1000, Chunks::none());
            assert!(odd.unwrap_err().to_string().contains("multiple of 8"));
            assert!(TableMut::create(base.add(8), len, &config, 1000, Chunks::none()).is_ok());
        }
        let mut chunk = vec![0_u64; 4];
        let chunk_base = chunk.as_mut_ptr().cast::<u8>();
        // SAFETY: the chunk pointers lie inside the vector; nothing is read.
        unsafe {
            let misaligned = [chunk_base.add(4)];
            assert!(Chunks::new(&misaligned, &[24]).is_err());
            let aligned = [chunk_base];
            assert!(Chunks::new(&aligned, &[20]).is_err(), "not a multiple of 8");
            assert!(
                Chunks::new(&aligned, &[0]).is_err(),
                "shorter than its used mark"
            );
            assert!(
                Chunks::new(&aligned, &[32, 32]).is_err(),
                "mismatched arrays"
            );
            assert!(Chunks::new(&aligned, &[32]).is_ok());
            assert!(init_chunk(chunk_base, 20).is_err());
            init_chunk(chunk_base, 32).unwrap();
        }
        assert_eq!(chunk[0], CHUNK_HEADER as u64);
    }
}
