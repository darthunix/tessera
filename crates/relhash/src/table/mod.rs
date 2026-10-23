//! The table: the index, the chunks and the calls over them. The crate's
//! root exports its items, and the crate's documentation describes them.
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
// The tests build their tables with the owning table. Two attributes, not
// one `all`: Clippy takes a module for tests by `cfg(test)` alone.
#[cfg(test)]
#[cfg(feature = "alloc")]
mod tests;
mod writer;

use core::marker::PhantomData;
use core::ops::Deref;

use crate::error::{Error, Kind, Result, Rule, ensure};
use crate::{RowMask, RowMaskView};

pub use batch::{Batch, Matches, MatchesView};
pub use chunks::{Appended, ChunkWriter, Chunks, MAX_PARTITIONS, Partitions, Split};
pub(crate) use header::MIN_BUCKETS;
pub use header::{
    CHUNK_HEADER, FORMAT_VERSION, HEADER_SIZE, MAX_CHUNK_LEN, MAX_CHUNKS, MAX_KEYS, TableConfig,
    UNIT_BITS, VERSION_OFFSET, index_size, record_bytes,
};
use header::{Header, Layout, NRECORDS};
pub use keys::KeySource;
pub use marks::{Marks, mark, mark_words, scan_unmarked};
#[cfg(feature = "alloc")]
pub use owned::OwnedTable;
use record::Access;
pub use record::{MAX_PAYLOAD_COLUMNS, PayloadColumns, Record, payload_null_words};
use region::{RawRegion, Region};
pub use writer::{Cursor, HELD, MergeStop, PayloadCursor};

/// What a table holds, for a caller that plans its memory or reports it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
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
fn other_config(config: &TableConfig) -> Error {
    Error::wrong_call(Rule::Config)
        .with(config.payload_size)
        .and(config.nkeys)
}

/// A table over an index and chunks, as several participants may share it.
///
/// It reads the table. Its calls that write chunks or the index, the
/// appends and the links, are `unsafe`: they are for the participants of
/// a shared build, each the one writer of its own chunks, and their
/// contracts say what the type cannot. [`TableMut`], which one writer has
/// to itself, makes the same calls safely.
///
/// Several participants, processes over shared memory or threads, may
/// use one table at once, each through a `Table` of its own over the
/// same blocks:
///
/// - append at once, each to chunks of its own: a chunk has one writer;
/// - link at once, each its own chunks;
/// - probe and read at once, once the records they may find are linked.
///
/// The calls of one writer, those of [`TableMut`], run alone. Of the
/// index and the chunks, only the count of records and the buckets change
/// while the table is shared, by atomic operations: a link adds to the
/// count before it publishes a record, and publishes it by a
/// compare-and-swap of its bucket with release; a probe reads a bucket
/// with acquire. So a participant that finds a record sees it whole.
/// [`Table::bloom`], [`Table::try_build_bloom`] and
/// [`Table::scan_unmarked`] read the used mark of every chunk: over a
/// table that several writers share, they run only once every writer's
/// appends are done, as [`Table::attach`] says.
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
unsafe fn init<R: Region>(region: &R, config: &TableConfig, capacity: u64) -> Result<Layout> {
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
        Error::wrong_call(Rule::IndexAlign)
    );
    ensure!(
        len >= HEADER_SIZE,
        Error::wrong_call(Rule::IndexShort).with(len)
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
    /// bytes for `'a`, and overlaps no chunk and neither array that names
    /// them. During `'a` the index and the chunks are accessed only
    /// through tables and chunk writers, here or in other processes
    /// mapping the same memory; probes run only once the records they may
    /// find are linked, and a chunk has one writer. [`Table::bloom`],
    /// [`Table::try_build_bloom`] and [`Table::scan_unmarked`], which read
    /// every chunk's used mark, run only once every writer's appends are
    /// done and ordered before them, as by a barrier.
    pub unsafe fn attach(index: *mut u8, len: usize, chunks: Chunks<'a>) -> Result<Self> {
        check_region(index, len)?;
        // SAFETY: the caller's contract, and `Chunks::new`'s for the chunks.
        let region = unsafe { chunks.region(index, len) };
        let layout = Header::load(&region).validate(len)?;
        Ok(Self {
            region,
            layout,
            _region: PhantomData,
        })
    }

    /// What the table was created with: its count of keys and its payload
    /// size. Inlined into the callers of other crates, as the kinds of the
    /// keys it replaces were.
    #[inline]
    pub fn config(&self) -> TableConfig {
        TableConfig {
            nkeys: self.layout.nkeys,
            payload_size: self.layout.payload_size,
        }
    }

    /// Bytes of payload per record.
    pub fn payload_size(&self) -> usize {
        self.layout.payload_size
    }

    /// Bytes of one record.
    pub fn record_size(&self) -> usize {
        self.layout.record_size
    }

    /// The fingerprint of the table's record layout. Blocks of records a
    /// table writes to disk carry it, and a table reads a block back only
    /// when its own fingerprint is the block's: records of another layout
    /// are refused, not read.
    pub fn fingerprint(&self) -> u64 {
        self.layout.fingerprint()
    }

    /// Refuse records of `config` unless its keys and payload size are the
    /// table's: a call that is given the layout of the records it writes,
    /// as an append is, checks it against the index when there is one, so
    /// that a caller's mistake is not written and found later as damage.
    pub fn check_config(&self, config: &TableConfig) -> Result<()> {
        if self.config() == *config {
            Ok(())
        } else {
            Err(other_config(config))
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
    /// zeros. The records are not counted or found until linked. The call
    /// checks the table's index, and refuses keys or a payload size other
    /// than the table's before it writes anything.
    ///
    /// # Safety
    ///
    /// The caller is the chunk's one writer: no other call writes it while
    /// this one runs, and no record of the chunk past its used mark is
    /// borrowed. [`TableMut::append`] is the same call, safe.
    pub unsafe fn append<K: KeySource + ?Sized>(
        &self,
        chunk: usize,
        payload: Option<&[u8]>,
        batch: &mut Batch<'_, K>,
    ) -> Result<usize> {
        batch::append(&self.region, &self.layout, chunk, payload, batch)
    }

    /// As [`Table::append`], with each row's payload taken from `columns`:
    /// a word of its NULL bits, then a word per column, which must be the
    /// table's whole payload.
    ///
    /// # Safety
    ///
    /// As [`Table::append`].
    pub unsafe fn append_columns<K: KeySource + ?Sized>(
        &self,
        chunk: usize,
        columns: &PayloadColumns<'_>,
        batch: &mut Batch<'_, K>,
    ) -> Result<usize> {
        batch::append_columns(&self.region, &self.layout, chunk, columns, batch)
    }

    /// Link the records of chunk `chunk` from byte `*from` to its used
    /// mark into their buckets, first in their chains, and move `*from`
    /// past them; the count linked is returned. Several participants may
    /// link at once, each its own chunks; equal keys make separate
    /// records. `*from` starts at [`CHUNK_HEADER`]. The count of records in
    /// the header grows before any of them can be found. A link that fails
    /// counts only the records it linked, and moves `*from` past them.
    ///
    /// # Safety
    ///
    /// The caller alone links the chunk, and no record of it from `*from`
    /// to its used mark is borrowed: linking writes their links.
    /// [`TableMut::link`] is the same call, safe.
    pub unsafe fn link(&self, chunk: usize, from: &mut usize) -> Result<usize> {
        batch::link(&self.region, &self.layout, chunk, from)
    }

    /// As [`Self::link`], also counting the records whose keys the table
    /// held already, each once, even while other participants link: a
    /// published record walks the rest of its chain for its keys. Returns
    /// the count linked and the duplicates.
    ///
    /// # Safety
    ///
    /// As [`Table::link`].
    pub unsafe fn link_counting(&self, chunk: usize, from: &mut usize) -> Result<(usize, usize)> {
        batch::link_counting::<_, true>(&self.region, &self.layout, chunk, from)
    }

    /// Find the first record of its chain with the hash, null bits and
    /// keys of each row of `matches`, whose reference goes to the row's
    /// slot; the rows without one leave `matches`, and the count of rows
    /// left is returned. The other records with the same keys follow
    /// through [`Table::next_match`], or [`Table::next_in_group`] in a
    /// table linked by [`TableMut::link_grouped`].
    pub fn probe<K: KeySource + ?Sized>(
        &self,
        hashes: &[u32],
        keys: &K,
        matches: &mut Matches<'_>,
    ) -> Result<usize> {
        let Matches { rows, refs, steps } = matches;
        *steps = 0;
        batch::probe(&self.region, &self.layout, hashes, keys, rows, refs)
    }

    /// Move each row of `matches` to the next record of its chain with
    /// the same hash, null bits and keys; the rows without one leave
    /// `matches` and keep their reference. The count of rows left is
    /// returned. A step past the table's count of records fails as a
    /// damaged chain, as [`Matches`] says.
    pub fn next_match(&self, matches: &mut Matches<'_>) -> Result<usize> {
        let Matches { rows, refs, steps } = matches;
        batch::next_match(&self.region, &self.layout, refs, rows, steps)
    }

    /// For each row of `matches`, the word at byte `at` of its record's
    /// payload into `out[row]`, native-endian: one word of a batch's
    /// matches per call, such as a column of the row a join keeps
    /// there. `at` must be a multiple of 8 and `at + 8` within the
    /// payload; the other rows keep their values in `out`.
    pub fn gather<'m>(
        &self,
        matches: impl Into<MatchesView<'m>>,
        at: usize,
        out: &mut [u64],
    ) -> Result<()> {
        self.gather_of(matches.into(), at, out)
    }

    /// As [`Table::gather`], for records in no order, as a sort reads
    /// them back: those of a word of rows are prefetched before any is
    /// read. Records just probed are in the cache, where [`Table::gather`]
    /// is faster.
    pub fn gather_scattered<'m>(
        &self,
        matches: impl Into<MatchesView<'m>>,
        at: usize,
        out: &mut [u64],
    ) -> Result<()> {
        self.gather_scattered_of(matches.into(), at, out)
    }

    /// For each row of `matches`, payload word `first + n` (in 8-byte
    /// units) of its record into `out[n][row]`, native-endian, for every
    /// `out[n]` of a row each, records in no order as
    /// [`Table::gather_scattered`] reads them: one call for every column a
    /// sort's batch serves, a record located once for all of them. The
    /// other rows keep their values.
    pub fn gather_words<'m>(
        &self,
        matches: impl Into<MatchesView<'m>>,
        first: usize,
        out: &mut [&mut [u64]],
    ) -> Result<()> {
        self.gather_words_of(matches.into(), first, out)
    }

    /// Move each row of `matches` to the record right after its own when
    /// that one has the same hash, null bits and keys; the rows without
    /// one leave `matches`, and the count of rows left is returned. In a
    /// table linked by [`TableMut::link_grouped`] this is the next record
    /// of the key, found in one step instead of a walk down the chain. A
    /// step past the table's count of records fails as for
    /// [`Table::next_match`].
    pub fn next_in_group(&self, matches: &mut Matches<'_>) -> Result<usize> {
        let Matches { rows, refs, steps } = matches;
        batch::next_in_group(&self.region, &self.layout, refs, rows, steps)
    }

    /// For each row of `matches`, key `key` of its record: its slot's bits
    /// into `values[row]` (the word the source of keys gave, 0 for a NULL)
    /// and whether it is NULL into `nulls[row]`. The other rows keep
    /// their values.
    pub fn gather_key<'m>(
        &self,
        matches: impl Into<MatchesView<'m>>,
        key: usize,
        values: &mut [u64],
        nulls: &mut [bool],
    ) -> Result<()> {
        self.gather_key_of(matches.into(), key, values, nulls)
    }

    // The gathers' bodies take no type parameter, so that they are
    // compiled here once, as before the gathers took `impl Into`, and not
    // in each caller, where they were inlined into its loops.

    fn gather_of(&self, matches: MatchesView<'_>, at: usize, out: &mut [u64]) -> Result<()> {
        let MatchesView { rows, refs } = matches;
        batch::gather::<_, false>(&self.region, &self.layout, refs, &rows, at, out)
    }

    fn gather_scattered_of(
        &self,
        matches: MatchesView<'_>,
        at: usize,
        out: &mut [u64],
    ) -> Result<()> {
        let MatchesView { rows, refs } = matches;
        batch::gather::<_, true>(&self.region, &self.layout, refs, &rows, at, out)
    }

    fn gather_words_of(
        &self,
        matches: MatchesView<'_>,
        first: usize,
        out: &mut [&mut [u64]],
    ) -> Result<()> {
        let MatchesView { rows, refs } = matches;
        batch::gather_words(&self.region, &self.layout, refs, &rows, first, out)
    }

    fn gather_key_of(
        &self,
        matches: MatchesView<'_>,
        key: usize,
        values: &mut [u64],
        nulls: &mut [bool],
    ) -> Result<()> {
        let MatchesView { rows, refs } = matches;
        batch::gather_key(&self.region, &self.layout, refs, &rows, key, values, nulls)
    }

    /// Fill a Bloom filter of [`bloom::words_for`] this table's records
    /// words (or any power of two) with the hash of every record of every
    /// chunk, linked or not, after clearing it: a probe row it rejects has
    /// no record with its hash. It reads every chunk's used mark, so over a
    /// shared table it runs once every writer's appends are done, as
    /// [`Table::attach`] says.
    pub fn bloom(&self, words: &mut [u64]) -> Result<()> {
        bloom::fill(&self.region, &self.layout, words)
    }

    /// Build a shared filter of this table's records unless another
    /// participant has claimed it: true for the one that built it. Call
    /// it, like [`Table::bloom`], once every writer's appends are done.
    /// A build that fails after it claimed the filter, at a damaged record,
    /// leaves it building: it never reads ready, the probes refuse it, and
    /// nobody waits for it.
    pub fn try_build_bloom(&self, filter: &bloom::SharedFilter<'_>) -> Result<bool> {
        bloom::try_build(&self.region, &self.layout, filter)
    }

    /// Set the marks of the records at `refs`, as [`mark`] does, over this
    /// table's chunks and with its record size, which a caller cannot get
    /// wrong.
    pub fn mark(&self, refs: &[u32], rows: &RowMaskView<'_>, marks: &impl Marks) -> Result<()> {
        let chunks = self.region.chunk_list();
        marks::mark(chunks.lens(), self.layout.record_size, refs, rows, marks)
    }

    /// Visit the records without a mark, as [`scan_unmarked`] does, over
    /// this table's chunks and with its record size. It reads the chunks'
    /// used marks: call it, like [`Table::bloom`], once every writer's
    /// appends are done.
    pub fn scan_unmarked(
        &self,
        cursor: &mut Cursor,
        marks: Option<&impl Marks>,
        out: &mut [u32],
    ) -> Result<usize> {
        let chunks = self.region.chunk_list();
        marks::scan_unmarked(&chunks, self.layout.record_size, cursor, marks, out)
    }

    /// The record at a reference a call of this table returned. A
    /// reference is checked against its chunk's length, not its used mark,
    /// which a writer moves: in a shared build one past the mark reads
    /// bytes that the chunk's writer may be writing, so a caller reads
    /// only the references the calls gave it. A reference that lands
    /// inside a record or on bytes no record starts at reads as a damaged
    /// table, [`Kind::Damaged`](crate::Kind::Damaged), whoever gave it:
    /// the crate cannot tell a caller's wrong reference from damage. The
    /// gathers and the steps to a next record read references alike.
    pub fn record(&self, reference: u32) -> Result<Record<'_>> {
        Ok(Access::new(&self.region, &self.layout)
            .locate(reference, Kind::WrongCall)?
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
}

/// A cursor that reads the records of a table by reference, made once a
/// call by [`Table::records`]: it keeps the table's access, whose fields
/// the loops hold in registers. The records it gives live as long as the
/// table's borrow.
pub struct RecordCursor<'t> {
    access: Access<'t, RawRegion>,
}

impl<'t> RecordCursor<'t> {
    /// The record at `reference`: an error for a reference that names no
    /// record of the table.
    #[inline(always)]
    pub fn record(&mut self, reference: u32) -> Result<Record<'t>> {
        Ok(self.access.locate(reference, Kind::WrongCall)?.record())
    }
}

impl<'a> TableMut<'a> {
    /// Create an empty table for `capacity` records with its index in the
    /// `len` bytes at `index`, which must be a multiple of 8 and at least
    /// [`index_size`], over `chunks`.
    ///
    /// # Safety
    ///
    /// As [`Table::attach`], and while this table exists nothing else
    /// reads or writes its index or its chunks: no other table, over this
    /// index or another, and no chunk writer.
    pub unsafe fn create(
        index: *mut u8,
        len: usize,
        config: &TableConfig,
        capacity: u64,
        chunks: Chunks<'a>,
    ) -> Result<Self> {
        check_region(index, len)?;
        // SAFETY: the caller's contract.
        let region = unsafe { chunks.region(index, len) };
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
    /// record references, the rows whose record this call created are
    /// added to `inserted`, whose other bits stay, and the count resolved
    /// is returned. Rows left pending need another chunk or a larger index
    /// ([`TableMut::regrow`]); an index has at most 2^31 buckets, so a
    /// table holds at most 2^30 records that a find or insert or a merge
    /// made. Over the calls of a batch `inserted` gathers
    /// every row that made a record: the caller clears it once, before the
    /// first.
    pub fn find_or_insert<K: KeySource + ?Sized>(
        &mut self,
        chunk: usize,
        batch: &mut Batch<'_, K>,
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
        batch: &mut Batch<'_, K>,
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
    /// record of new keys needs another chunk or a larger index, at most
    /// 2^30 records as for [`TableMut::find_or_insert`]; the count merged
    /// is returned. When the rule fails, the call returns its error, the
    /// records before it merged, and the payload it was changing as the
    /// rule left it: a rule that must leave it whole changes it only once
    /// it cannot fail.
    ///
    /// Chunk `source` is one of the table's chunks while it merges, so the
    /// calls over every chunk would take its records for the table's: it
    /// leaves the chunks, or is reset, before a larger index, a filter, a
    /// cleared key or a walk.
    pub fn merge<E: From<Error>>(
        &mut self,
        source: usize,
        from: &mut usize,
        chunk: usize,
        rule: impl FnMut(&[u8], &mut [u8]) -> Result<(), E>,
    ) -> Result<(usize, MergeStop), E> {
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

    /// As [`Table::append`], over a table this writer has to itself.
    pub fn append<K: KeySource + ?Sized>(
        &mut self,
        chunk: usize,
        payload: Option<&[u8]>,
        batch: &mut Batch<'_, K>,
    ) -> Result<usize> {
        // SAFETY: the writer has the table to itself, and a record borrowed
        // from it would hold this handle.
        unsafe { self.0.append(chunk, payload, batch) }
    }

    /// As [`Table::append_columns`], over a table this writer has to
    /// itself.
    pub fn append_columns<K: KeySource + ?Sized>(
        &mut self,
        chunk: usize,
        columns: &PayloadColumns<'_>,
        batch: &mut Batch<'_, K>,
    ) -> Result<usize> {
        // SAFETY: as in `append`.
        unsafe { self.0.append_columns(chunk, columns, batch) }
    }

    /// As [`Table::link`], over a table this writer has to itself.
    pub fn link(&mut self, chunk: usize, from: &mut usize) -> Result<usize> {
        // SAFETY: as in `append`.
        unsafe { self.0.link(chunk, from) }
    }

    /// As [`Table::link_counting`], over a table this writer has to
    /// itself.
    pub fn link_counting(&mut self, chunk: usize, from: &mut usize) -> Result<(usize, usize)> {
        // SAFETY: as in `append`.
        unsafe { self.0.link_counting(chunk, from) }
    }

    /// Visit the records from `cursor` on, chunk by chunk in the order
    /// they were appended, as many as `out` holds: their references fill
    /// `out`, the count is returned and the cursor moves past them; 0
    /// means the walk is over. It visits every record below the used
    /// marks, linked or not. A walk reads the chunks' used marks, which
    /// writers move, so it needs the table to itself.
    pub fn scan(&self, cursor: &mut Cursor, out: &mut [u32]) -> Result<usize> {
        writer::scan(&self.0.region, &self.0.layout, cursor, out)
    }

    /// A cursor over the payloads of the table's records, to change them
    /// by reference, one at a time: made once a call, it reads the table's
    /// header once.
    #[inline(always)]
    pub fn payloads(&mut self) -> PayloadCursor<'_> {
        PayloadCursor::new(&mut self.0)
    }

    /// The payload of the record at a reference, to change in place.
    pub fn payload_mut(&mut self, reference: u32) -> Result<&mut [u8]> {
        writer::payload_mut(&self.0.region, &self.0.layout, reference)
    }

    /// Hand `each` the row and the payload of the record of each selected
    /// row, in the order of the rows, to change in place: the payloads of
    /// a batch, which hold the references [`TableMut::find_or_insert`]
    /// gave them, for one check of the table. `refs` has a reference
    /// per row of the mask; a reference that names no record fails the
    /// call, after the rows before it.
    pub fn each_payload(
        &mut self,
        refs: &[u32],
        rows: &RowMaskView<'_>,
        each: impl FnMut(usize, &mut [u8]),
    ) -> Result<()> {
        writer::each_payload(&self.0.region, &self.0.layout, refs, rows, each)
    }

    /// Write into `out`, at each selected row, a raw pointer to the
    /// payload of its record, for a caller that changes the payloads of a
    /// batch after the call, as C does. `refs` and `out` have a slot per
    /// row of the mask; each reference is checked as a lookup checks it,
    /// and one that names no record fails the call, after the rows before
    /// it. Two rows of one record get the same pointer.
    ///
    /// The pointers are made from the chunks' bases, not from loans of the
    /// payloads, so each may read and write its payload beside the others
    /// until the chunk is reset or the blocks' lifetime ends. Using them is
    /// the caller's `unsafe`: while it does, no record or payload of those
    /// bytes is borrowed through a table, and nothing else writes them.
    /// [`Self::each_payload`] lends the payloads one at a time instead, for
    /// a caller that is done with each before the next.
    pub fn payload_ptrs(
        &mut self,
        refs: &[u32],
        rows: &RowMaskView<'_>,
        out: &mut [*mut u8],
    ) -> Result<()> {
        writer::payload_ptrs(&self.0.region, &self.0.layout, refs, rows, out)
    }

    /// Write 0 into key `key` of every record of the chunks, linked or not,
    /// its NULL bit kept; the count of records is returned. The key then orders no two records that are
    /// not NULL, and a probe by it no longer finds them: for records nothing
    /// looks up by their keys, such as a sort's.
    pub fn clear_key(&mut self, key: usize) -> Result<u64> {
        writer::clear_key(&self.0.region, &self.0.layout, key)
    }

    /// Move the table to a new index of `len` bytes at `index`, for
    /// `capacity` records, over the same chunks: every record is linked
    /// there again, grouped chains stay grouped, and the old index is no
    /// longer the table's. Records and their references stay as they were.
    /// An index has at most 2^31 buckets, however large the capacity, and
    /// a new index with fewer buckets than the old is refused.
    ///
    /// The chunks are the table's whole set of records: a record appended
    /// but not linked yet is linked here too, and a link of it afterwards
    /// pushes it onto its own chain, a cycle of one record. A merge's
    /// source leaves the chunks before.
    ///
    /// # Safety
    ///
    /// As [`TableMut::create`] for the new index, which must live for
    /// `'a` and overlap neither the old one nor the chunks.
    pub unsafe fn regrow(&mut self, index: *mut u8, len: usize, capacity: u64) -> Result<()> {
        check_region(index, len)?;
        // SAFETY: the caller's contract.
        let to = unsafe { self.0.region.with_index(index, len) };
        self.0.layout = writer::regrow(&self.0.region, &to, &self.0.layout, capacity)?;
        self.0.region = to;
        Ok(())
    }
}
