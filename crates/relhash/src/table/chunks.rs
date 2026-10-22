//! The chunks of a table, the blocks that hold its records: the checks
//! of a block that is to be one, and the calls over chunks alone, before
//! the table has an index, by which the participants of a shared build
//! append their share and a spill copies a chunk's records to partitions.

use core::marker::PhantomData;

use crate::error::{Error, Result, Rule, ensure};

use super::batch::{self, Batch};
use super::header::{
    self, CHUNK_HEADER, Layout, MAX_CHUNK_LEN, MAX_CHUNKS, TableConfig, check_record_size,
};
use super::keys::KeySource;
use super::record::PayloadColumns;
use super::region::RawRegion;

/// What a call that appends to partitions counts: the rows appended to
/// each partition, at its number in `rows`, and the NULL bits of every row
/// appended, or-ed into `nulls`. The counts add up over calls.
pub struct Appended<'a> {
    /// The rows appended to each partition, at its number.
    pub rows: &'a mut [u64],
    /// The NULL bits of every row appended, or-ed.
    pub nulls: &'a mut u64,
}

/// Whether a block can be a chunk: not null, aligned to 8, and a multiple
/// of 8 of [`CHUNK_HEADER`] to [`MAX_CHUNK_LEN`] bytes. A call checks it of
/// every chunk it writes or walks, when it starts on the chunk.
#[inline(always)]
pub(super) fn chunk_fits(base: *const u8, len: usize) -> bool {
    // The length's rule is `chunk_len_fits`'s, written out: through that
    // function the alignments of the base and of the length were tested
    // apart, two branches more on every chunk a call starts on.
    !base.is_null()
        && base.addr().is_multiple_of(8)
        && len.is_multiple_of(8)
        && (CHUNK_HEADER..=MAX_CHUNK_LEN).contains(&len)
}

/// Whether a chunk can have `len` bytes: a multiple of 8 of
/// [`CHUNK_HEADER`] to [`MAX_CHUNK_LEN`]. The owning table checks its
/// chunks' length by it.
#[cfg(feature = "alloc")]
#[inline(always)]
pub(super) fn chunk_len_fits(len: usize) -> bool {
    len.is_multiple_of(8) && (CHUNK_HEADER..=MAX_CHUNK_LEN).contains(&len)
}

/// The error of a chunk of a table that cannot be one.
#[cold]
#[inline(never)]
pub(super) fn bad_chunk(chunk: usize, len: usize) -> Error {
    Error::wrong_call(Rule::Chunk).with(chunk).and(len)
}

/// The error of a block a caller gave as a chunk that cannot be one.
#[cold]
#[inline(never)]
fn bad_block(len: usize) -> Error {
    Error::wrong_call(Rule::Block).with(len)
}

/// The used mark `raw` of a chunk of `len` bytes whose records take
/// `record_size` bytes, when it can be one: at least the mark's own 8
/// bytes, at most the chunk's length, and the end of a whole number of
/// records.
#[inline(always)]
pub(super) fn used_mark(raw: u64, len: usize, record_size: usize) -> Option<usize> {
    usize::try_from(raw).ok().filter(|&used| {
        used >= CHUNK_HEADER && used <= len && (used - CHUNK_HEADER).is_multiple_of(record_size)
    })
}

/// The chunks of a table as this process sees them: each one's base and
/// length.
///
/// The bases are raw pointers, not slices. In shared memory other
/// processes write a table's chunks while a call runs, which a
/// `&mut [u64]` of them would forbid; only the caller can promise what
/// the memory is, so [`Self::new`] is unsafe. [`OwnedTable`](super::OwnedTable)
/// is the way to a table without `unsafe`.
#[derive(Clone, Copy, Debug)]
pub struct Chunks<'a> {
    pub(super) bases: &'a [*mut u8],
    pub(super) lens: &'a [usize],
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
    /// length for `'a`. No two chunks overlap, and none overlaps `bases`,
    /// `lens` or the index of a table over them. During `'a` the chunks
    /// are accessed only through tables and chunk writers, here or in
    /// other processes mapping the same memory, and a chunk has one
    /// writer at a time across all of them. [`scan_unmarked`](super::scan_unmarked),
    /// and the calls of a table that read every chunk's used mark, run
    /// only once every writer's appends are done and ordered before them,
    /// as by a barrier.
    pub unsafe fn new(bases: &'a [*mut u8], lens: &'a [usize]) -> Result<Self> {
        ensure!(
            bases.len() == lens.len() && bases.len() <= MAX_CHUNKS,
            Error::wrong_call(Rule::Chunks)
                .with(bases.len())
                .and(lens.len())
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

    /// The memory of a table over these chunks and the index of `len`
    /// bytes at `index`.
    ///
    /// # Safety
    ///
    /// The index is as [`Table::attach`](super::Table::attach) asks, or
    /// empty for a call that never reads it.
    pub(super) unsafe fn region(&self, index: *mut u8, len: usize) -> RawRegion {
        // SAFETY: the caller's contract for the index, and `Self::new`'s
        // for the chunks.
        unsafe {
            RawRegion::new(
                index,
                len,
                self.bases.as_ptr(),
                self.lens.as_ptr(),
                self.len(),
            )
        }
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

    /// Make a block of `len` bytes at `base` an empty chunk: its used mark
    /// covers only itself.
    ///
    /// # Safety
    ///
    /// `base` is aligned to 8 and valid for writes of `len` bytes, a
    /// multiple of 8 of [`CHUNK_HEADER`] to [`MAX_CHUNK_LEN`], and nothing
    /// else uses the block yet.
    pub unsafe fn init(base: *mut u8, len: usize) -> Result<()> {
        if !chunk_fits(base, len) {
            return Err(bad_block(len));
        }
        // SAFETY: the caller's contract; the used mark is the first word.
        unsafe { base.cast::<u64>().write(CHUNK_HEADER as u64) };
        Ok(())
    }

    /// The used mark of the chunk of `len` bytes at `base`, whose records
    /// take `record_size` bytes each: checked to lie between
    /// [`CHUNK_HEADER`] and `len` and to end a record, so that a caller
    /// outside the crate never reads the mark by itself.
    ///
    /// # Safety
    ///
    /// `base` is valid for reads of `len` bytes when the block can be a
    /// chunk, and nothing writes its first word during the call.
    pub unsafe fn used(base: *const u8, len: usize, record_size: usize) -> Result<usize> {
        if !chunk_fits(base, len) {
            return Err(bad_block(len));
        }
        check_record_size(record_size)?;
        // SAFETY: the caller's contract; the used mark is the chunk's first
        // word, aligned as the block is.
        let used = unsafe { base.cast::<u64>().read() };
        used_mark(used, len, record_size).ok_or(
            Error::damaged(Rule::BlockUsedMark)
                .with(used)
                .and(record_size),
        )
    }
}

/// The chunks alone as a region, with an empty index, which the calls
/// over chunks never read.
fn chunk_region(chunks: &Chunks<'_>) -> RawRegion {
    // SAFETY: an empty index, and chunks aligned and valid by the contract
    // of `Chunks::new`; a call checks each chunk it starts on.
    unsafe { chunks.region(core::ptr::NonNull::<u64>::dangling().as_ptr().cast(), 0) }
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
    /// The bit of a hash where the partition's bits start.
    pub shift: u32,
    /// The chunk each partition appends to, a power of two of them.
    pub chunks: &'a [u32],
}

/// What [`ChunkWriter::split`] did: the records copied, and the partition whose
/// chunk was full, if it stopped at one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct Split {
    /// The records copied.
    pub count: usize,
    /// The partition whose chunk was full, if the split stopped at one.
    pub full: Option<u32>,
}

/// A table's chunks before the table has an index, and the calls that
/// write records to them: the participants of a shared build append
/// their share, and one of them sizes the index once all are counted; a
/// spill copies a chunk's records to the chunks of their partitions. It
/// checks the configuration once, when it is made.
///
/// Its calls write chunks that a table over the same chunks may read, so
/// it is made by an `unsafe` call, whose contract makes it the one writer
/// of the chunks it writes:
///
/// ```compile_fail,E0133
/// use relhash::{ChunkWriter, Chunks, TableConfig};
///
/// let config = TableConfig { nkeys: 1, payload_size: 8 };
/// // A chunk writer made in safe code: refused.
/// let writer = ChunkWriter::new(&config, Chunks::none())?;
/// # Ok::<(), relhash::Error>(())
/// ```
#[derive(Debug)]
pub struct ChunkWriter<'a> {
    region: RawRegion,
    layout: Layout,
    _chunks: PhantomData<&'a ()>,
}

impl<'a> ChunkWriter<'a> {
    /// The writer of records of a table of `config` to `chunks`.
    ///
    /// # Safety
    ///
    /// While the writer lives, it is the one writer of each chunk its
    /// calls write: no table or other chunk writer, here or in another
    /// process, writes that chunk, and no record or payload of it is
    /// borrowed while one of its calls runs.
    pub unsafe fn new(config: &TableConfig, chunks: Chunks<'a>) -> Result<Self> {
        Ok(Self {
            layout: header::chunk_layout(config)?,
            region: chunk_region(&chunks),
            _chunks: PhantomData,
        })
    }

    /// Append the rows of `batch` as records to chunk `chunk`, as
    /// [`Table::append`](super::Table::append) does.
    pub fn append<K: KeySource + ?Sized>(
        &self,
        chunk: usize,
        payload: Option<&[u8]>,
        batch: &mut Batch<'_, K>,
    ) -> Result<usize> {
        batch::append(&self.region, &self.layout, chunk, payload, batch)
    }

    /// As [`Self::append`], with each row's payload taken from `columns`: a
    /// word of its NULL bits, then a word per column, which must be the
    /// table's whole payload.
    pub fn append_columns<K: KeySource + ?Sized>(
        &self,
        chunk: usize,
        columns: &PayloadColumns<'_>,
        batch: &mut Batch<'_, K>,
    ) -> Result<usize> {
        batch::append_columns(&self.region, &self.layout, chunk, columns, batch)
    }

    /// Append the rows of `batch` as records, each to the chunk of its
    /// hash's partition: as [`Self::append_columns`], except that a row
    /// whose partition's chunk is full stays pending while the rows after
    /// it go on; every row appended counts in `appended`.
    pub fn append_partitioned_columns<K: KeySource + ?Sized>(
        &self,
        partitions: &Partitions<'_>,
        columns: &PayloadColumns<'_>,
        batch: &mut Batch<'_, K>,
        appended: &mut Appended<'_>,
    ) -> Result<usize> {
        batch::append_partitioned_columns(
            &self.region,
            &self.layout,
            partitions,
            columns,
            batch,
            appended,
        )
    }

    /// Copy the records of chunk `source` from byte `*from` on, whole and
    /// in order, each to the chunk of its hash's partition, and move
    /// `*from` past them: at most `refs.len()`, their new references into
    /// `refs` and their hashes into `hashes`, stopping before a record
    /// whose partition's chunk is full. The copies are not linked; `*from`
    /// starts at [`CHUNK_HEADER`].
    pub fn split(
        &self,
        partitions: &Partitions<'_>,
        source: usize,
        from: &mut usize,
        refs: &mut [u32],
        hashes: &mut [u32],
    ) -> Result<Split> {
        batch::split(
            &self.region,
            &self.layout,
            partitions,
            source,
            from,
            refs,
            hashes,
        )
    }
}
