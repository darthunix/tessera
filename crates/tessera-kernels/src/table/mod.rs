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
//! place, or walk the records ([`TableMut::scan`]). Hashes come from
//! [`crate::int32::hash`], [`crate::int64::hash`] and their `hash_next`,
//! which decide what NULL keys do.
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
//! use tessera_kernels::table::{Batch, KeyKind, LocalTable, TableConfig};
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
//! let mut batch = Batch::new(&hashes, &keys[..], &mut mask, &mut offsets)?;
//! table.insert(Some(&payload), &mut batch)?;
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
mod marks;
mod order;
pub mod phases;
mod record;
mod region;
pub mod shared_spill;

use core::marker::PhantomData;
use core::ops::Deref;

use anyhow::{Result, anyhow, ensure};
use tessera_core::{ColumnReader, RowMask, RowMaskView};

pub use exclusive::{Combine, CombineStop, Cursor, ExtremeSlot, Fold, MAX_SUMS, Slot, SumSlot};
pub use header::{
    CHUNK_HEADER, FORMAT_VERSION, HEADER_SIZE, KeyKind, MAX_CHUNK_LEN, MAX_CHUNKS, MAX_KEYS,
    TableConfig, UNIT_BITS, VERSION_OFFSET, index_size, record_bytes, record_bytes_of,
};
use header::{Header, Layout, NRECORDS};
pub use keys::{KeySource, KeyValue, normalize_word};
pub use local::LocalTable;
pub use marks::{Marks, mark, mark_words, scan_unmarked};
use record::Access;
pub use record::{MAX_PAYLOAD_COLUMNS, PayloadColumns, Record, payload_null_words};
use region::{RawRegion, Region};

/// The rows of a call that places them in a table: each row's hash and
/// keys, the rows still to place, which leave `pending` as the call
/// places them, and the reference of each placed row's record, at its
/// row in `offsets`. A batch may go through several calls, each placing
/// what it can.
pub struct Batch<'b, 'm, K: ?Sized> {
    hashes: &'b [u32],
    keys: &'b K,
    pending: &'b mut RowMask<'m>,
    offsets: &'b mut [u32],
}

impl<'b, 'm, K: KeySource + ?Sized> Batch<'b, 'm, K> {
    /// The rows of `pending`, whose row count the keys, the hashes and the
    /// offsets must have.
    pub fn new(
        hashes: &'b [u32],
        keys: &'b K,
        pending: &'b mut RowMask<'m>,
        offsets: &'b mut [u32],
    ) -> Result<Self> {
        let nrows = pending.as_view().nrows();
        ensure!(
            keys.nrows() == nrows && hashes.len() == nrows && offsets.len() == nrows,
            "the keys, hashes, mask and offsets of the batch have different row counts"
        );
        Ok(Self {
            hashes,
            keys,
            pending,
            offsets,
        })
    }

    /// The rows still to place.
    pub fn pending(&self) -> RowMaskView<'_> {
        self.pending.as_view()
    }
}

/// What a call that appends to partitions counts: the rows appended to
/// each partition, at its number in `rows`, and the NULL bits of every row
/// appended, or-ed into `nulls`. The counts add up over calls.
pub struct Appended<'a> {
    pub rows: &'a mut [u64],
    pub nulls: &'a mut u64,
}

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

/// Whether a block can be a chunk: not null, aligned to 8, and a multiple
/// of 8 of [`CHUNK_HEADER`] to [`MAX_CHUNK_LEN`] bytes. A call checks it of
/// every chunk it writes or walks, when it starts on the chunk.
#[inline(always)]
pub(super) fn chunk_fits(base: *const u8, len: usize) -> bool {
    !base.is_null()
        && base.addr().is_multiple_of(8)
        && len.is_multiple_of(8)
        && (CHUNK_HEADER..=MAX_CHUNK_LEN).contains(&len)
}

/// The error of a chunk that cannot be one.
#[cold]
#[inline(never)]
pub(super) fn bad_chunk(chunk: usize, len: usize) -> anyhow::Error {
    anyhow!(
        "table chunk {chunk} of {len} bytes is not aligned to 8 or not a multiple of 8 \
         of {CHUNK_HEADER} to {MAX_CHUNK_LEN} bytes"
    )
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
    /// the arrays match and there are at most [`MAX_CHUNKS`]. The chunks
    /// themselves are not checked here: a call checks each chunk it writes
    /// or walks when it starts on it, and the others are the caller's
    /// promise, as the validity of their memory is. Checking every chunk at
    /// every call cost a call a fixed part that grew with the table;
    /// [`Self::check_all`] still does it, for debug builds.
    ///
    /// # Safety
    ///
    /// Every base is aligned to 8 and valid for reads and writes of its
    /// length for `'a`, and during `'a` the chunks are accessed only
    /// through tables, here or in other processes mapping the same memory.
    pub unsafe fn new(bases: &'a [*mut u8], lens: &'a [usize]) -> Result<Self> {
        ensure!(
            bases.len() == lens.len() && bases.len() <= MAX_CHUNKS,
            "a table has at most {MAX_CHUNKS} chunks with a length each, not {} bases and {} lengths",
            bases.len(),
            lens.len()
        );
        Ok(Self { bases, lens })
    }

    /// Check that every chunk can be one, as a call checks the chunks it
    /// writes or walks: for a debug build of the entry points, which then
    /// finds a caller's wrong chunk even where no call touches it.
    pub fn check_all(&self) -> Result<()> {
        for (chunk, (&base, &len)) in self.bases.iter().zip(self.lens).enumerate() {
            if !chunk_fits(base, len) {
                return Err(bad_chunk(chunk, len));
            }
        }
        Ok(())
    }

    /// Whether chunk `chunk`, below the count, can be one.
    #[inline]
    pub(super) fn fits(&self, chunk: usize) -> bool {
        chunk_fits(self.bases[chunk], self.lens[chunk])
    }

    /// The number of chunks.
    pub fn len(&self) -> usize {
        self.bases.len()
    }

    /// The chunks' lengths.
    pub fn lens(&self) -> &'a [usize] {
        self.lens
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
        chunk_fits(base, len),
        "a table chunk of {len} bytes is not aligned to 8 or not a multiple of 8 of \
         {CHUNK_HEADER} to {MAX_CHUNK_LEN} bytes"
    );
    // SAFETY: the caller's contract; the used mark is the first word.
    unsafe { base.cast::<u64>().write(CHUNK_HEADER as u64) };
    Ok(())
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

/// The chunks alone as a region, with an empty index, which the calls
/// over chunks never read.
fn chunk_region(chunks: &Chunks<'_>) -> RawRegion {
    // SAFETY: an empty index, and chunks aligned and valid by the contract
    // of `Chunks::new`; a call checks each chunk it starts on.
    unsafe {
        RawRegion::new(
            core::ptr::NonNull::<u64>::dangling().as_ptr().cast(),
            0,
            chunks.bases.as_ptr(),
            chunks.lens.as_ptr(),
            chunks.len(),
        )
    }
}

/// Append the rows of `batch` as records of a table of `config` to chunk
/// `chunk` of `chunks`, as [`Table::append`] does, before the table has an
/// index: the participants of a shared build append their share first,
/// and one of them sizes the index for the records once all are counted.
/// The caller must be the chunk's one writer.
pub fn append_to<K: KeySource + ?Sized>(
    config: &TableConfig<'_>,
    chunks: Chunks<'_>,
    chunk: usize,
    payload: Option<&[u8]>,
    batch: &mut Batch<'_, '_, K>,
) -> Result<usize> {
    let layout = header::chunk_layout(config)?;
    batch::append(&chunk_region(&chunks), &layout, chunk, payload, batch)
}

/// As [`append_to`], with each row's payload taken from `columns`: a word
/// of its NULL bits, then a word per column, which must be the table's
/// whole payload.
pub fn append_columns_to<K: KeySource + ?Sized>(
    config: &TableConfig<'_>,
    chunks: Chunks<'_>,
    chunk: usize,
    columns: &PayloadColumns<'_>,
    batch: &mut Batch<'_, '_, K>,
) -> Result<usize> {
    let layout = header::chunk_layout(config)?;
    batch::append_columns(&chunk_region(&chunks), &layout, chunk, columns, batch)
}

/// The most partitions one split makes.
pub const MAX_PARTITIONS: usize = 1 << 16;

/// Where the records of a table that spills go: the partition of a hash
/// is `(hash >> shift) & (chunks.len() - 1)`, a power of two of
/// partitions, and each partition appends to its chunk of `chunks`. The
/// buckets take the hash's high bits, so a table spills by its low ones,
/// the next level of partitions by the bits above them.
#[derive(Clone, Copy, Debug)]
pub struct Partitions<'a> {
    pub shift: u32,
    pub chunks: &'a [u32],
}

/// What [`split_to`] did: the records copied, and the partition whose
/// chunk was full, if it stopped at one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Split {
    pub count: usize,
    pub full: Option<u32>,
}

/// Append the rows of `batch` as records of a table of `config`, each to
/// the chunk of its hash's partition, over the chunks alone: as
/// [`append_columns_to`], each row's payload taken from `columns`, except
/// that a row whose partition's chunk is full stays pending while the rows
/// after it go on; every row appended counts in `appended`. The caller
/// must be the one writer of every partition's chunk.
pub fn append_partitioned_columns_to<K: KeySource + ?Sized>(
    config: &TableConfig<'_>,
    chunks: Chunks<'_>,
    partitions: &Partitions<'_>,
    columns: &PayloadColumns<'_>,
    batch: &mut Batch<'_, '_, K>,
    appended: &mut Appended<'_>,
) -> Result<usize> {
    let layout = header::chunk_layout(config)?;
    batch::append_partitioned_columns(
        &chunk_region(&chunks),
        &layout,
        partitions,
        columns,
        batch,
        appended,
    )
}

/// Copy the records of chunk `source` of a table of `config` from byte
/// `*from` on, whole and in order, each to the chunk of its hash's
/// partition, and move `*from` past them: at most `offsets.len()`, their
/// new references into `offsets` and their hashes into `hashes`, stopping before a record whose partition's chunk is
/// full. The copies are not linked; `*from` starts at [`CHUNK_HEADER`].
/// The caller must be the one writer of every partition's chunk.
pub fn split_to(
    config: &TableConfig<'_>,
    chunks: Chunks<'_>,
    partitions: &Partitions<'_>,
    source: usize,
    from: &mut usize,
    offsets: &mut [u32],
    hashes: &mut [u32],
) -> Result<Split> {
    let layout = header::chunk_layout(config)?;
    batch::split(
        &chunk_region(&chunks),
        &layout,
        partitions,
        source,
        from,
        offsets,
        hashes,
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

    /// The sort items of every record of every chunk, linked or not, in
    /// the order appended, one after another in `items` (see
    /// [`crate::sort`]): `keys` order the table's keys, one each, of its
    /// kinds. Returns the count; [`crate::sort::sort_items`] then sorts
    /// them and gives the references in order. The chunks must not change
    /// meanwhile.
    pub fn sort_items(&self, keys: &[crate::sort::SortKey], items: &mut [u64]) -> Result<usize> {
        order::items(&self.region, &self.layout, keys, items)
    }

    /// Push the item of each record `refs[row]` of `rows` into a top-N
    /// heap of `heap.len() / words` items, whose first `*len` are the heap
    /// (see [`crate::sort::top_candidates`]); the records need not be
    /// linked.
    pub fn top_push(
        &self,
        keys: &[crate::sort::SortKey],
        refs: &[u32],
        rows: &RowMaskView<'_>,
        heap: &mut [u64],
        len: &mut usize,
    ) -> Result<()> {
        order::top_push(&self.region, &self.layout, keys, refs, rows, heap, len)
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
        exclusive::find_or_insert(&self.0.region, &self.0.layout, chunk, batch, inserted)
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
        exclusive::find_or_insert_partitioned(
            &self.0.region,
            &self.0.layout,
            partitions,
            batch,
            inserted,
        )
    }

    /// Merge the records of chunk `source` from byte `*from` on, each a
    /// group's states as [`Combine`] says per aggregate after a word of
    /// flags, into the records of the same keys, copying a group the table
    /// lacks to chunk `chunk`; `*from` moves past those merged, and the
    /// call stops where a new group needs another chunk or a larger index.
    /// For a grouping that spilled, reading a partition back.
    pub fn combine(
        &mut self,
        source: usize,
        from: &mut usize,
        chunk: usize,
        combines: &[Combine],
    ) -> Result<(usize, CombineStop)> {
        exclusive::combine(
            &self.0.region,
            &self.0.layout,
            source,
            from,
            chunk,
            combines,
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

    /// Write 0 into key `key` of every record, its NULL bit kept; the count
    /// of records is returned. The key then orders no two records that are
    /// not NULL, and a probe by it no longer finds them: for records nothing
    /// looks up by their keys, such as a sort's.
    pub fn clear_key(&mut self, key: usize) -> Result<u64> {
        exclusive::clear_key(&self.0.region, &self.0.layout, key)
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

    /// Fold each selected row's terms into the sum or average states of
    /// its record's payload ([`crate::decimal::SumState`]), the record
    /// found once a row: the rows a state does not take go to its sum's
    /// rest for the caller.
    pub fn sum_terms<T: crate::decimal::Terms>(
        &mut self,
        offsets: &[u32],
        rows: &RowMaskView<'_>,
        sums: &mut [SumSlot<'_, T>],
    ) -> Result<()> {
        exclusive::sum_terms(&self.0.region, &self.0.layout, offsets, rows, sums)
    }

    /// Merge each selected row's partial states into the sum or average
    /// states of its record's payload ([`crate::decimal::SumState::merge`]),
    /// the record found once a row: the states the table does not merge go
    /// to their sum's rest for the caller.
    pub fn sum_partials<P: crate::decimal::Partials>(
        &mut self,
        offsets: &[u32],
        rows: &RowMaskView<'_>,
        sums: &mut [SumSlot<'_, P>],
    ) -> Result<()> {
        exclusive::sum_partials(&self.0.region, &self.0.layout, offsets, rows, sums)
    }

    /// Offer each selected row's term to the `min` or `max` of numeric
    /// state of its record's payload ([`crate::decimal::ExtremeState`]), in
    /// the rows' order: the rows it does not decide, and its group's later
    /// rows of the batch, go to the slot's rest for the caller.
    pub fn extremes<T: crate::decimal::Terms>(
        &mut self,
        offsets: &[u32],
        rows: &RowMaskView<'_>,
        slot: &mut ExtremeSlot<'_, T>,
    ) -> Result<()> {
        exclusive::extremes(&self.0.region, &self.0.layout, offsets, rows, slot)
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
            .insert(
                None,
                &mut Batch::new(&hashes, &column[..], &mut pending, &mut offsets).unwrap(),
            )
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

    /// Chunks of words the tests of partitioning add as they fill, each
    /// read and written only through its pointer, taken once.
    struct Blocks {
        _storage: Vec<Vec<u64>>,
        bases: Vec<*mut u8>,
        lens: Vec<usize>,
    }

    impl Blocks {
        fn new() -> Self {
            Self {
                _storage: Vec::new(),
                bases: Vec::new(),
                lens: Vec::new(),
            }
        }

        fn add(&mut self, len: usize) -> u32 {
            // A vector, not a box: moving a box would retag its memory as
            // unique and retire the pointer taken here.
            let mut block = vec![0_u64; len / 8];
            let base = block.as_mut_ptr().cast::<u8>();
            // SAFETY: the block is aligned to 8, `len` bytes, used only here.
            unsafe { init_chunk(base, len) }.unwrap();
            self._storage.push(block);
            self.bases.push(base);
            self.lens.push(len);
            (self.bases.len() - 1) as u32
        }

        fn chunks(&self) -> Chunks<'_> {
            // SAFETY: the blocks live as long as `self` and are accessed
            // only through tables and `records`.
            unsafe { Chunks::new(&self.bases, &self.lens) }.unwrap()
        }

        /// The (hash, next, key, payload) of every record of a chunk of
        /// records of one int4 key and one payload word.
        fn records(&self, chunk: u32) -> Vec<(u32, u32, i64, u64)> {
            let base = self.bases[chunk as usize].cast::<u64>();
            // SAFETY: the used mark and the records below it lie in the block.
            unsafe {
                let used = base.read() as usize;
                (CHUNK_HEADER..used)
                    .step_by(32)
                    .map(|byte| {
                        let record = base.add(byte / 8);
                        let header = record.read();
                        (
                            header as u32,
                            (header >> 32) as u32,
                            record.add(2).read() as i64,
                            record.add(3).read(),
                        )
                    })
                    .collect()
            }
        }
    }

    const PARTITION_CONFIG: TableConfig<'static> = TableConfig {
        keys: &[KeyKind::Int32],
        payload_size: 8,
    };

    /// 200 rows, the key's square as payload.
    fn partition_rows() -> (Vec<i32>, Vec<u32>, Vec<u8>) {
        let keys: Vec<i32> = (0..200).collect();
        let hashes = keys.iter().map(|&key| murmurhash32(key as u32)).collect();
        let payload = keys
            .iter()
            .flat_map(|&key| (key as u64 * key as u64).to_ne_bytes())
            .collect();
        (keys, hashes, payload)
    }

    /// Every row once, each in a chunk of its hash's partition, whole.
    fn check_partitioned(blocks: &Blocks, owner: &[u32], shift: u32, keys: &[i32]) {
        let mut seen = vec![false; keys.len()];
        for (chunk, &partition) in owner.iter().enumerate() {
            if partition == u32::MAX {
                continue;
            }
            for (hash, next, key, payload) in blocks.records(chunk as u32) {
                assert_eq!(
                    (hash >> shift) & 3,
                    partition,
                    "record in another partition"
                );
                assert_eq!(hash, murmurhash32(key as u32));
                assert_eq!(payload, (key * key) as u64);
                assert_eq!(next, 0);
                assert!(!seen[key as usize], "key {key} twice");
                seen[key as usize] = true;
            }
        }
        assert!(seen.iter().all(|&seen| seen), "a row is missing");
    }

    #[test]
    fn a_chunk_splits_into_the_chunks_of_its_partitions() {
        let (keys, hashes, payload) = partition_rows();
        let column = [ColumnView::try_new(&keys, None).unwrap()];
        let mut blocks = Blocks::new();
        let source = blocks.add(CHUNK_HEADER + 200 * 32);
        let mut words = vec![u64::MAX, u64::MAX, u64::MAX, (1 << 8) - 1];
        let mut pending = RowMask::try_new(200, &mut words).unwrap();
        let mut offsets = vec![0; 200];
        append_to(
            &PARTITION_CONFIG,
            blocks.chunks(),
            source as usize,
            Some(&payload),
            &mut Batch::new(&hashes, &column[..], &mut pending, &mut offsets).unwrap(),
        )
        .unwrap();
        let len = CHUNK_HEADER + 8 * 32;
        let mut current: Vec<u32> = (0..4).map(|_| blocks.add(len)).collect();
        let mut owner = vec![u32::MAX, 0, 1, 2, 3];
        let mut from = CHUNK_HEADER;
        let mut copied = 0;
        let mut new_offsets = [0; 16];
        let mut split_hashes = [0; 16];
        loop {
            let partitions = Partitions {
                shift: 9,
                chunks: &current,
            };
            let split = split_to(
                &PARTITION_CONFIG,
                blocks.chunks(),
                &partitions,
                source as usize,
                &mut from,
                &mut new_offsets,
                &mut split_hashes,
            )
            .unwrap();
            for (&offset, &hash) in new_offsets.iter().zip(&split_hashes).take(split.count) {
                assert_eq!(owner[(offset >> UNIT_BITS) as usize], (hash >> 9) & 3);
            }
            copied += split.count;
            match split.full {
                Some(partition) => {
                    current[partition as usize] = blocks.add(len);
                    owner.push(partition);
                }
                None if split.count == 0 => break,
                None => {}
            }
        }
        assert_eq!(copied, 200);
        assert_eq!(from, CHUNK_HEADER + 200 * 32);
        check_partitioned(&blocks, &owner, 9, &keys);
    }

    #[test]
    fn partitions_past_the_hash_or_the_chunks_are_refused() {
        let mut blocks = Blocks::new();
        // Four chunks for partitions and a fifth, the source of a split.
        for _ in 0..5 {
            blocks.add(CHUNK_HEADER + 256 * 32);
        }
        let cases: [(u32, &[u32], &str); 4] = [
            (0, &[0, 1, 2], "power of two"),
            (31, &[0, 1, 2, 3], "past the 32 bits"),
            (0, &[0, 1, 2, 9], "does not exist"),
            (32, &[0], "past the 32 bits"),
        ];
        for (shift, chunks, message) in cases {
            let mut from = CHUNK_HEADER;
            let error = split_to(
                &PARTITION_CONFIG,
                blocks.chunks(),
                &Partitions { shift, chunks },
                4,
                &mut from,
                &mut [0; 4],
                &mut [0; 4],
            )
            .unwrap_err();
            assert!(error.to_string().contains(message), "{error}");
        }
        let mut from = CHUNK_HEADER;
        let error = split_to(
            &PARTITION_CONFIG,
            blocks.chunks(),
            &Partitions {
                shift: 0,
                chunks: &[0, 1],
            },
            1,
            &mut from,
            &mut [0; 4],
            &mut [0; 4],
        )
        .unwrap_err();
        assert!(error.to_string().contains("into itself"), "{error}");
    }

    /// A split of no records would answer as the end of its chunk does;
    /// one into a partition's chunk, or of records of another size, would
    /// copy what it must not. Each fails, and nothing moves.
    #[test]
    fn a_split_that_cannot_be_done_is_refused() {
        let (keys, hashes, payload) = partition_rows();
        let column = [ColumnView::try_new(&keys, None).unwrap()];
        let mut blocks = Blocks::new();
        let source = blocks.add(CHUNK_HEADER + 200 * 32);
        let mut words = vec![u64::MAX, u64::MAX, u64::MAX, (1 << 8) - 1];
        let mut pending = RowMask::try_new(200, &mut words).unwrap();
        let mut offsets = vec![0; 200];
        append_to(
            &PARTITION_CONFIG,
            blocks.chunks(),
            source as usize,
            Some(&payload),
            &mut Batch::new(&hashes, &column[..], &mut pending, &mut offsets).unwrap(),
        )
        .unwrap();
        let targets: Vec<u32> = (0..4).map(|_| blocks.add(CHUNK_HEADER + 64 * 32)).collect();
        let wider = TableConfig {
            keys: &[KeyKind::Int32],
            payload_size: 16,
        };
        let cases: [(&TableConfig<'_>, usize, u32, &str); 3] = [
            (&PARTITION_CONFIG, 0, targets[0], "at least one record"),
            (&PARTITION_CONFIG, 16, source, "into itself"),
            (&wider, 16, targets[0], "record"),
        ];
        for (config, capacity, first, message) in cases {
            let chunks = [first, targets[1], targets[2], targets[3]];
            let mut from = CHUNK_HEADER;
            let error = split_to(
                config,
                blocks.chunks(),
                &Partitions {
                    shift: 9,
                    chunks: &chunks,
                },
                source as usize,
                &mut from,
                &mut vec![0; capacity],
                &mut vec![0; capacity],
            )
            .unwrap_err();
            assert!(error.to_string().contains(message), "{error}");
            assert_eq!(from, CHUNK_HEADER, "{message}");
            for &target in &targets {
                assert!(blocks.records(target).is_empty(), "{message}: copied");
            }
        }
    }

    /// Keys and their hashes as a batch's key column.
    fn key_batch(keys: &[i32]) -> (Vec<u32>, Vec<u64>) {
        let hashes = keys.iter().map(|&key| murmurhash32(key as u32)).collect();
        let mut words = vec![0; keys.len().div_ceil(64)];
        for row in 0..keys.len() {
            words[row / 64] |= 1 << (row % 64);
        }
        (hashes, words)
    }

    #[test]
    fn groups_go_to_the_chunks_of_their_partitions() {
        let config = TableConfig {
            keys: &[KeyKind::Int32],
            payload_size: 8,
        };
        // Records of 32 bytes, 8 to a chunk: the partitions fill.
        let mut table = LocalTable::new(&config, 1024, CHUNK_HEADER + 8 * 32).unwrap();
        let mut current: Vec<u32> = (0..4).map(|_| table.add_chunk().unwrap() as u32).collect();
        let mut owner: Vec<u32> = (0..4).collect();
        let mut first = std::collections::HashMap::new();
        for batch in 0..3 {
            let keys: Vec<i32> = (0..100).map(|row| (batch * 100 + row) % 97).collect();
            let column = [ColumnView::try_new(&keys, None).unwrap()];
            let (hashes, all) = key_batch(&keys);
            let mut pending_words = all.clone();
            let mut offsets = vec![0; keys.len()];
            loop {
                let mut inserted_words = vec![0; all.len()];
                let mut pending = RowMask::try_new(keys.len(), &mut pending_words).unwrap();
                let mut inserted = RowMask::try_new(keys.len(), &mut inserted_words).unwrap();
                table
                    .table_mut()
                    .unwrap()
                    .find_or_insert_partitioned(
                        &Partitions {
                            shift: 3,
                            chunks: &current,
                        },
                        &mut Batch::new(&hashes, &column[..], &mut pending, &mut offsets).unwrap(),
                        &mut inserted,
                    )
                    .unwrap();
                if pending_words.iter().all(|&word| word == 0) {
                    break;
                }
                let mut full = [false; 4];
                for row in 0..keys.len() {
                    if pending_words[row / 64] >> (row % 64) & 1 == 1 {
                        full[((hashes[row] >> 3) & 3) as usize] = true;
                    }
                }
                for partition in 0..4 {
                    if full[partition] {
                        current[partition] = table.add_chunk().unwrap() as u32;
                        owner.push(partition as u32);
                    }
                }
            }
            let reader = table.table().unwrap();
            for (row, &key) in keys.iter().enumerate() {
                let record = reader.record(offsets[row]).unwrap();
                assert_eq!(record.keys[0], i64::from(key));
                assert_eq!(
                    owner[(offsets[row] >> UNIT_BITS) as usize],
                    (hashes[row] >> 3) & 3,
                    "a group outside its partition"
                );
                assert_eq!(*first.entry(key).or_insert(offsets[row]), offsets[row]);
            }
        }
        assert_eq!(table.table().unwrap().stats().records, 97);
    }

    /// The states of a group: flags, count, sum, min, max.
    fn states(table: &LocalTable, offset: u32) -> [i64; 5] {
        let reader = table.table().unwrap();
        let record = reader.record(offset).unwrap();
        let mut out = [0; 5];
        for (index, word) in out.iter_mut().enumerate() {
            *word =
                i64::from_ne_bytes(record.payload[8 * index..8 * index + 8].try_into().unwrap());
        }
        out
    }

    /// A table of the keys, each with the states `of` gives it.
    fn grouped(
        keys: &[i32],
        of: impl Fn(i32) -> [i64; 5],
        capacity: u64,
    ) -> (LocalTable, Vec<u32>) {
        let config = TableConfig {
            keys: &[KeyKind::Int32],
            payload_size: 40,
        };
        let mut table = LocalTable::new(&config, capacity, CHUNK_HEADER + 16 * 56).unwrap();
        let column = [ColumnView::try_new(keys, None).unwrap()];
        let (hashes, mut pending_words) = key_batch(keys);
        let mut inserted_words = vec![0; pending_words.len()];
        let mut offsets = vec![0; keys.len()];
        table
            .find_or_insert(
                &mut Batch::new(
                    &hashes,
                    &column[..],
                    &mut RowMask::try_new(keys.len(), &mut pending_words).unwrap(),
                    &mut offsets,
                )
                .unwrap(),
                &mut RowMask::try_new(keys.len(), &mut inserted_words).unwrap(),
            )
            .unwrap();
        for (row, &key) in keys.iter().enumerate() {
            let mut writer = table.table_mut().unwrap();
            let payload = writer.payload_mut(offsets[row]).unwrap();
            for (index, word) in of(key).iter().enumerate() {
                payload[8 * index..8 * index + 8].copy_from_slice(&word.to_ne_bytes());
            }
        }
        (table, offsets)
    }

    const COMBINES: [Combine; 4] = [Combine::Count, Combine::Sum, Combine::Min, Combine::Max];

    /// Merge every chunk of `from` into `into`, adding chunks and growing
    /// its index as the kernel asks.
    fn merge(into: &mut LocalTable, from: &mut LocalTable) -> Result<()> {
        let mut chunk = into.chunks() - 1;
        for source_chunk in 0..from.chunks() {
            let words = from.chunk_words(source_chunk).to_vec();
            let source = into.add_chunk_copy(&words)?;
            let mut at = CHUNK_HEADER;
            loop {
                let (_, stop) = into
                    .table_mut()?
                    .combine(source, &mut at, chunk, &COMBINES)?;
                match stop {
                    CombineStop::Done => break,
                    CombineStop::ChunkFull => chunk = into.add_chunk()?,
                    CombineStop::IndexFull => {
                        let records = into.table()?.stats().records;
                        into.regrow(records * 2)?;
                    }
                }
            }
        }
        Ok(())
    }

    #[test]
    fn states_of_a_group_merge() {
        // Some groups on both sides, with and without values of their own.
        let ours = |key: i32| {
            let key = i64::from(key);
            let flags = if key % 5 == 0 { 0 } else { 0b1110 };
            [flags, key, key * 10, key, key]
        };
        let theirs = |key: i32| {
            let key = i64::from(key);
            let flags = if key % 3 == 0 { 0 } else { 0b1110 };
            [flags, 1, key, key - 100, key + 100]
        };
        let (mut into, _) = grouped(&(0..50).collect::<Vec<_>>(), ours, 64);
        let (mut from, _) = grouped(&(25..100).collect::<Vec<_>>(), theirs, 256);
        merge(&mut into, &mut from).unwrap();
        let keys: Vec<i32> = (0..100).collect();
        let column = [ColumnView::try_new(&keys, None).unwrap()];
        let (hashes, mut pending_words) = key_batch(&keys);
        let mut inserted_words = vec![0; pending_words.len()];
        let mut offsets = vec![0; keys.len()];
        into.find_or_insert(
            &mut Batch::new(
                &hashes,
                &column[..],
                &mut RowMask::try_new(keys.len(), &mut pending_words).unwrap(),
                &mut offsets,
            )
            .unwrap(),
            &mut RowMask::try_new(keys.len(), &mut inserted_words).unwrap(),
        )
        .unwrap();
        assert!(
            inserted_words.iter().all(|&word| word == 0),
            "a group was lost"
        );
        for &key in &keys {
            let (a, b) = (ours(key), theirs(key));
            let (in_a, in_b) = (key < 50, key >= 25);
            let expected = match (in_a, in_b) {
                (true, false) => a,
                (false, true) => b,
                _ => {
                    let mut merged = [a[0] | b[0], a[1] + b[1], a[2], a[3], a[4]];
                    let (fa, fb) = (a[0] != 0, b[0] != 0);
                    if fa && fb {
                        merged[2] = a[2] + b[2];
                        merged[3] = a[3].min(b[3]);
                        merged[4] = a[4].max(b[4]);
                    } else if fb {
                        merged[2..].copy_from_slice(&b[2..]);
                    }
                    merged
                }
            };
            assert_eq!(states(&into, offsets[key as usize]), expected, "key {key}");
        }
    }

    #[test]
    fn a_merged_count_past_the_int8_range_fails() {
        let (mut into, _) = grouped(&[7], |_| [0, i64::MAX, 0, 0, 0], 64);
        let (mut from, _) = grouped(&[7], |_| [0, 1, 0, 0, 0], 64);
        let error = merge(&mut into, &mut from).unwrap_err();
        assert!(error.to_string().contains("bigint out of range"), "{error}");
        let (mut table, _) = grouped(&[7], |_| [0; 5], 64);
        let chunk = table.chunks() - 1;
        let mut at = CHUNK_HEADER;
        let error = table
            .table_mut()
            .unwrap()
            .combine(chunk, &mut at, chunk, &COMBINES)
            .unwrap_err();
        assert!(error.to_string().contains("into itself"), "{error}");
    }

    #[test]
    fn a_damaged_count_is_bounded_by_the_places_of_the_chunks() {
        let config = TableConfig {
            keys: &[KeyKind::Int32],
            payload_size: 8,
        };
        let mut table = LocalTable::new(&config, 10, 4096).unwrap();
        table.add_chunk().unwrap();
        table.add_chunk().unwrap();
        // The header counts more records than the chunks could ever hold:
        // a walk takes for its bound the places a reference can name in
        // the two chunks.
        table.index_words()[NRECORDS / 8] = u64::MAX;
        let table = table.table().unwrap();
        let access = Access::new(&table.region, &table.layout);
        assert_eq!(access.records(), 2 << UNIT_BITS);
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
            let short = index_size(&config, 1000).unwrap() - 8;
            let short = TableMut::create(base, short, &config, 1000, Chunks::none());
            assert!(short.unwrap_err().to_string().contains("smaller than"));
            assert!(TableMut::create(base.add(8), len, &config, 1000, Chunks::none()).is_ok());
        }
        // Blocks whose used mark reads as an empty chunk's, so that only
        // the check of the chunk refuses them: one aligned, one 4 bytes in.
        let mut block = vec![0_u64; 8];
        let block_base = block.as_mut_ptr().cast::<u8>();
        // SAFETY: every base and length lies inside the vector, which
        // nothing else uses meanwhile.
        unsafe {
            block_base.cast::<u64>().write(CHUNK_HEADER as u64);
            let misaligned = block_base.add(32);
            misaligned
                .add(4)
                .cast::<u64>()
                .write_unaligned(CHUNK_HEADER as u64);
            let misaligned = misaligned.add(4);
            let cases = [
                (misaligned, 24, "misaligned"),
                (block_base, 28, "not a multiple of 8"),
                (block_base, 0, "shorter than its used mark"),
                (core::ptr::null_mut(), 24, "null"),
            ];
            for (base, len, what) in cases {
                let bases = [base];
                let lens = [len];
                // The array is taken as it is; the chunk, when a call
                // writes or walks it.
                let chunks = Chunks::new(&bases, &lens).unwrap();
                assert!(chunks.check_all().is_err(), "{what}");
                let appended = append_one(chunks);
                assert!(
                    appended.is_err_and(|error| error.to_string().contains("not aligned to 8")),
                    "an append to a chunk {what}"
                );
            }
            assert!(
                Chunks::new(&[block_base], &[32, 32]).is_err(),
                "mismatched arrays"
            );
            let bases = [block_base];
            let lens = [32];
            let chunks = Chunks::new(&bases, &lens).unwrap();
            chunks.check_all().unwrap();
            assert_eq!(append_one(chunks).unwrap(), 1);
            assert!(init_chunk(block_base, 20).is_err());
            init_chunk(block_base, 32).unwrap();
        }
        assert_eq!(block[0], CHUNK_HEADER as u64);
    }

    /// Append the row of key 7 to chunk 0 of `chunks`, in a table of an
    /// int4 key and no payload, records of 24 bytes: the rows appended.
    fn append_one(chunks: Chunks<'_>) -> Result<usize> {
        let keys = [7];
        let column = [ColumnView::try_new(&keys[..], None)?];
        let (hashes, mut words) = key_batch(&keys);
        let mut pending = RowMask::try_new(1, &mut words)?;
        let mut offsets = [0];
        let config = TableConfig {
            keys: &[KeyKind::Int32],
            payload_size: 0,
        };
        append_to(
            &config,
            chunks,
            0,
            None,
            &mut Batch::new(&hashes, &column[..], &mut pending, &mut offsets)?,
        )
    }

    #[test]
    fn chunks_past_the_limits_are_refused() {
        // One block as long as the longest chunk and a word more, and as
        // many chunks as a table has and one more, each the block's used
        // mark alone.
        let mut words = vec![0_u64; (MAX_CHUNK_LEN + 8) / 8];
        let base = words.as_mut_ptr().cast::<u8>();
        let bases = vec![base; MAX_CHUNKS + 1];
        let lens = vec![CHUNK_HEADER; MAX_CHUNKS + 1];
        // SAFETY: every base is the vector's start, valid for the lengths
        // given; only a chunk of the block alone is written.
        unsafe {
            base.cast::<u64>().write(CHUNK_HEADER as u64);
            // A chunk longer than 1 MiB has places that no reference can
            // name: refused when a call starts on it.
            let one = [base];
            let too_long = [MAX_CHUNK_LEN + 8];
            let longest = Chunks::new(&one, &too_long).unwrap();
            assert!(longest.check_all().is_err());
            assert!(
                append_one(longest)
                    .is_err_and(|error| error.to_string().contains("not aligned to 8")),
                "an append to a chunk longer than 1 MiB"
            );
            assert!(init_chunk(base, MAX_CHUNK_LEN + 8).is_err());
            let long = [MAX_CHUNK_LEN];
            let longest = Chunks::new(&one, &long).unwrap();
            longest.check_all().unwrap();
            assert_eq!(append_one(longest).unwrap(), 1);
            assert!(Chunks::new(&bases[..MAX_CHUNKS], &lens[..MAX_CHUNKS]).is_ok());
            assert!(
                Chunks::new(&bases, &lens).is_err(),
                "more chunks than a reference can number"
            );
        }
    }
}
