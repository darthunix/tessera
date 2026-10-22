//! A table that owns its index and chunks, for tests, benchmarks and any
//! caller in one process that wants no `unsafe`.
//!
//! The blocks are Rust allocations held as raw pointers from their boxes,
//! so that tables over them may write through the pointers while this
//! owner is only borrowed; they are freed when it drops. Chunks are added
//! as they fill and never move, and a larger index replaces the old one
//! when the records outgrow it, as a node does with its memory.

use crate::error::{Error, Result, Rule, ensure};
use crate::{RowMask, RowMaskView};

use super::{
    Batch, CHUNK_HEADER, Chunks, KeySource, MAX_CHUNKS, Table, TableConfig, TableMut, index_size,
};

/// A block of words owned through a raw pointer.
#[derive(Debug)]
struct Block {
    words: *mut u64,
    len: usize,
}

impl Block {
    fn new(words: usize) -> Self {
        let boxed = vec![0u64; words].into_boxed_slice();
        let len = boxed.len();
        Self {
            words: Box::into_raw(boxed).cast::<u64>(),
            len,
        }
    }

    fn bytes(&self) -> usize {
        self.len * 8
    }

    fn base(&self) -> *mut u8 {
        self.words.cast()
    }

    /// The words, for a test to read or damage.
    fn words_mut(&mut self) -> &mut [u64] {
        // SAFETY: the block owns `len` initialized words, and the exclusive
        // borrow keeps every table over it away meanwhile.
        unsafe { core::slice::from_raw_parts_mut(self.words, self.len) }
    }
}

impl Drop for Block {
    fn drop(&mut self) {
        // SAFETY: the pointer came from `Box::into_raw` of this length.
        drop(unsafe { Box::from_raw(core::ptr::slice_from_raw_parts_mut(self.words, self.len)) });
    }
}

/// A table with its index and chunks in this process's memory.
///
/// Its calls may come in any order: an insertion links the records it
/// appended and no others, a find or insert links each record it makes
/// at once, and a larger index or a reset keeps the table whole. Chunks
/// too small for a record are refused when the table is made.
///
/// A record read from the table borrows it, and the calls that write
/// need it to themselves, so a record borrowed across an append does not
/// compile:
///
/// ```compile_fail
/// use relhash::{KeySource, OwnedTable, RowMaskView, TableConfig, ones};
///
/// struct Keys<'a>(&'a [i32]);
/// impl KeySource for Keys<'_> {
///     fn nkeys(&self) -> usize { 1 }
///     fn nrows(&self) -> usize { self.0.len() }
///     fn word(&self, _: usize, index: usize, selected: u64, out: &mut [i64; 64]) -> Option<u64> {
///         for bit in ones(selected) {
///             out[bit] = i64::from(self.0[index * 64 + bit]);
///         }
///         Some(selected)
///     }
/// }
///
/// let mut table = OwnedTable::new(&TableConfig { nkeys: 1, payload_size: 8 }, 4, 4096)?;
/// let one = RowMaskView::try_new(1, &[1])?;
/// let mut refs = [0];
/// table.insert(&[7], &Keys(&[7]), one, None, &mut refs)?;
/// let shared = table.table()?;
/// let record = shared.record(refs[0])?;
/// // A record borrowed across an append: refused.
/// table.insert(&[8], &Keys(&[8]), one, None, &mut refs)?;
/// assert_eq!(record.keys, [7]);
/// # Ok::<(), relhash::Error>(())
/// ```
#[derive(Debug)]
pub struct OwnedTable {
    index: Block,
    chunks: Vec<Block>,
    bases: Vec<*mut u8>,
    lens: Vec<usize>,
    /// Bytes of each new chunk.
    chunk_bytes: usize,
    /// The words of the rows a call has still to place, kept between calls
    /// so that a call allocates them once.
    pending: Vec<u64>,
    /// The capacity the index was last sized for.
    capacity: u64,
}

impl OwnedTable {
    /// An empty table whose index is sized for `capacity` records and
    /// whose chunks will have `chunk_bytes` bytes each.
    pub fn new(config: &TableConfig, capacity: u64, chunk_bytes: usize) -> Result<Self> {
        ensure!(
            super::chunks::chunk_len_fits(chunk_bytes),
            Error::wrong_call(Rule::Block).with(chunk_bytes)
        );
        let index = Block::new(index_size(config, capacity)?.div_ceil(8));
        // SAFETY: the block is this table's alone, aligned to 8 and of the
        // length given; there is no chunk yet.
        unsafe {
            TableMut::create(
                index.base(),
                index.bytes(),
                config,
                capacity,
                Chunks::none(),
            )
        }?;
        let table = Self {
            index,
            chunks: Vec::new(),
            bases: Vec::new(),
            lens: Vec::new(),
            chunk_bytes,
            pending: Vec::new(),
            capacity,
        };
        // A chunk must hold a record, or the calls that add chunks as they
        // fill would add them to the limit.
        ensure!(
            table.table()?.record_size() <= chunk_bytes - CHUNK_HEADER,
            Error::wrong_call(Rule::RecordRoom).with(chunk_bytes)
        );
        Ok(table)
    }

    /// Empty the table, keeping its index and chunks, for a caller that
    /// builds it again.
    pub fn reset(&mut self) -> Result<()> {
        let config = self.table()?.config();
        // SAFETY: the blocks are this owner's, exclusively borrowed, and
        // were created of these lengths.
        unsafe {
            TableMut::create(
                self.index.base(),
                self.index.bytes(),
                &config,
                self.capacity,
                Chunks::none(),
            )?;
            for block in &self.chunks {
                Chunks::init(block.base(), block.bytes())?;
            }
        }
        Ok(())
    }

    /// The chunks, as a caller passes them without the index.
    pub(super) fn chunk_set(&self) -> Result<Chunks<'_>> {
        // SAFETY: the owned chunks are valid for their lengths while `self`
        // lives, each a block of its own apart from the arrays and the
        // index, and only tables over this owner access them.
        unsafe { Chunks::new(&self.bases, &self.lens) }
    }

    /// The table, shared.
    pub fn table(&self) -> Result<Table<'_>> {
        // SAFETY: the index and chunks are owned and live as long as the
        // borrow.
        unsafe { Table::attach(self.index.base(), self.index.bytes(), self.chunk_set()?) }
    }

    /// The table, for its one writer.
    pub fn table_mut(&mut self) -> Result<TableMut<'_>> {
        // SAFETY: as for `table`, with the owner borrowed exclusively.
        unsafe { TableMut::attach_mut(self.index.base(), self.index.bytes(), self.chunk_set()?) }
    }

    /// Add an empty chunk and return its number.
    pub fn add_chunk(&mut self) -> Result<usize> {
        ensure!(
            self.chunks.len() < MAX_CHUNKS,
            Error::wrong_call(Rule::ChunkLimit)
        );
        let block = Block::new(self.chunk_bytes / 8);
        // SAFETY: the block is new, aligned to 8 and of a checked length.
        unsafe { Chunks::init(block.base(), block.bytes()) }?;
        self.bases.push(block.base());
        self.lens.push(block.bytes());
        self.chunks.push(block);
        Ok(self.chunks.len() - 1)
    }

    /// Add a chunk holding a copy of `words`, a chunk's words from another
    /// table of the same layout, such as one read back from disk, and
    /// return its number; its records are not linked.
    #[cfg(test)]
    pub(super) fn add_chunk_copy(&mut self, words: &[u64]) -> Result<usize> {
        ensure!(
            self.chunks.len() < MAX_CHUNKS,
            Error::wrong_call(Rule::ChunkLimit)
        );
        let mut block = Block::new(words.len());
        block.words_mut().copy_from_slice(words);
        self.bases.push(block.base());
        self.lens.push(block.bytes());
        self.chunks.push(block);
        Ok(self.chunks.len() - 1)
    }

    /// The number of chunks.
    pub fn chunks(&self) -> usize {
        self.chunks.len()
    }

    /// Move the table to a new index sized for `capacity` records.
    pub fn regrow(&mut self, capacity: u64) -> Result<()> {
        let config_len = index_size(&self.table()?.config(), capacity)?;
        let index = Block::new(config_len.div_ceil(8));
        {
            let mut table = self.table_mut()?;
            // SAFETY: the new block is this owner's, aligned to 8, and it
            // becomes the table's index below, for as long as the owner.
            unsafe { table.regrow(index.base(), index.bytes(), capacity) }?;
        }
        self.index = index;
        self.capacity = capacity;
        Ok(())
    }

    /// Append the rows of `rows` to the last chunk, adding chunks as they
    /// fill, and link them: each row gets the reference of its record in
    /// `refs`, at its row, and the count inserted is returned. `hashes`,
    /// `keys` and `payload` are those of every row of the batch, as for
    /// [`Batch`] and [`Table::append`]; `rows` stays as it is.
    pub fn insert<K: KeySource + ?Sized>(
        &mut self,
        hashes: &[u32],
        keys: &K,
        rows: RowMaskView<'_>,
        payload: Option<&[u8]>,
        refs: &mut [u32],
    ) -> Result<usize> {
        let (first, start) = self.append_start();
        let appended = self.place(hashes, keys, rows, refs, |table, batch| {
            table.append_all(payload, batch)
        })?;
        let chunks = self.chunks.len();
        let mut table = self.table_mut()?;
        for chunk in first..chunks {
            let mut from = if chunk == first { start } else { CHUNK_HEADER };
            table.link(chunk, &mut from)?;
        }
        Ok(appended)
    }

    /// As [`Self::insert`], linking by [`TableMut::link_grouped`]: the
    /// records of a key lie next to each other. Returns the count inserted
    /// and, of them, those whose keys the table held already.
    #[cfg(test)]
    pub(super) fn insert_grouped<K: KeySource + ?Sized>(
        &mut self,
        hashes: &[u32],
        keys: &K,
        rows: RowMaskView<'_>,
        payload: Option<&[u8]>,
        refs: &mut [u32],
    ) -> Result<(usize, usize)> {
        let (first, start) = self.append_start();
        let appended = self.place(hashes, keys, rows, refs, |table, batch| {
            table.append_all(payload, batch)
        })?;
        let chunks = self.chunks.len();
        let mut table = self.table_mut()?;
        let mut duplicates = 0;
        for chunk in first..chunks {
            let mut from = if chunk == first { start } else { CHUNK_HEADER };
            duplicates += table.link_grouped(chunk, &mut from)?.1;
        }
        Ok((appended, duplicates))
    }

    /// Make a batch of the rows of `rows` over the table's own words of
    /// pending rows, and give it to `call`, which places every row.
    fn place<K: KeySource + ?Sized, T>(
        &mut self,
        hashes: &[u32],
        keys: &K,
        rows: RowMaskView<'_>,
        refs: &mut [u32],
        call: impl FnOnce(&mut Self, &mut Batch<'_, K>) -> Result<T>,
    ) -> Result<T> {
        let mut pending = core::mem::take(&mut self.pending);
        pending.clear();
        pending.extend(rows.words());
        let result = RowMask::try_new(rows.nrows(), &mut pending)
            .and_then(|mask| Batch::from_mask(hashes, keys, mask, refs))
            .and_then(|mut batch| call(self, &mut batch));
        self.pending = pending;
        result
    }

    /// Where an append starts: the last chunk and its used mark, or the
    /// start of the first chunk, which the append adds. The insertions
    /// link from there what they appended, and nothing else, whatever
    /// other calls appended, linked or found before.
    fn append_start(&mut self) -> (usize, usize) {
        let chunks = self.chunks.len();
        match self.chunks.last_mut() {
            Some(last) => (chunks - 1, last.words_mut()[0] as usize),
            None => (0, CHUNK_HEADER),
        }
    }

    /// Append every pending row of `batch`, adding chunks as they fill.
    fn append_all<K: KeySource + ?Sized>(
        &mut self,
        payload: Option<&[u8]>,
        batch: &mut Batch<'_, K>,
    ) -> Result<usize> {
        let mut appended = 0;
        loop {
            if self.chunks.is_empty() {
                self.add_chunk()?;
            }
            let chunk = self.chunks.len() - 1;
            let count = self.table_mut()?.append(chunk, payload, batch)?;
            appended += count;
            if batch.pending().selected_count() == 0 {
                return Ok(appended);
            }
            ensure!(
                count > 0 || !self.chunk_is_empty(chunk),
                Error::wrong_call(Rule::RecordRoom).with(self.chunk_bytes)
            );
            self.add_chunk()?;
        }
    }

    fn chunk_is_empty(&mut self, chunk: usize) -> bool {
        self.chunks[chunk].words_mut()[0] == CHUNK_HEADER as u64
    }

    /// Give each row of `rows` the record of its keys, as
    /// [`TableMut::find_or_insert`] does, adding chunks and building a
    /// larger index as needed: each row gets the reference of its record
    /// in `refs`, at its row, the rows whose record this call created are
    /// added to `inserted`, and the count resolved is returned. `rows`
    /// stays as it is.
    pub fn find_or_insert<K: KeySource + ?Sized>(
        &mut self,
        hashes: &[u32],
        keys: &K,
        rows: RowMaskView<'_>,
        refs: &mut [u32],
        inserted: &mut RowMask<'_>,
    ) -> Result<usize> {
        self.place(hashes, keys, rows, refs, |table, batch| {
            table.resolve_all(batch, inserted)
        })
    }

    /// Resolve every pending row of `batch`, adding chunks and building a
    /// larger index as they fill.
    fn resolve_all<K: KeySource + ?Sized>(
        &mut self,
        batch: &mut Batch<'_, K>,
        inserted: &mut RowMask<'_>,
    ) -> Result<usize> {
        let mut resolved = 0;
        loop {
            if self.chunks.is_empty() {
                self.add_chunk()?;
            }
            let chunk = self.chunks.len() - 1;
            resolved += self.table_mut()?.find_or_insert(chunk, batch, inserted)?;
            if batch.pending().selected_count() == 0 {
                return Ok(resolved);
            }
            let table = self.table()?;
            let records = table.stats().records;
            if records >= table.layout.most_records() {
                self.regrow(records * 2)?;
            } else {
                self.add_chunk()?;
            }
        }
    }

    /// The index's words, for a test to read or damage.
    #[cfg(test)]
    pub(super) fn index_words(&mut self) -> &mut [u64] {
        self.index.words_mut()
    }

    /// A chunk's words, for a test to read or damage.
    #[cfg(test)]
    pub(super) fn chunk_words(&mut self, chunk: usize) -> &mut [u64] {
        self.chunks[chunk].words_mut()
    }
}
