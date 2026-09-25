//! A table that owns its index and chunks, for tests, benchmarks and any
//! caller in one process that wants no `unsafe`.
//!
//! The blocks are Rust allocations held as raw pointers from their boxes,
//! so that tables over them may write through the pointers while this
//! owner is only borrowed; they are freed when it drops. Chunks are added
//! as they fill and never move, and a larger index replaces the old one
//! when the records outgrow it, as a node does with its memory.

use anyhow::{Result, ensure};
use tessera_core::RowMask;

use super::{
    CHUNK_HEADER, Chunks, KeySource, MAX_CHUNK_LEN, MAX_CHUNKS, Table, TableConfig, TableMut,
    index_size, init_chunk,
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
#[derive(Debug)]
pub struct LocalTable {
    index: Block,
    chunks: Vec<Block>,
    bases: Vec<*mut u8>,
    lens: Vec<usize>,
    /// Bytes of each new chunk.
    chunk_bytes: usize,
    /// Per chunk: the byte linking has reached.
    linked: Vec<usize>,
    /// The capacity the index was last sized for.
    capacity: u64,
}

impl LocalTable {
    /// An empty table whose index is sized for `capacity` records and
    /// whose chunks will have `chunk_bytes` bytes each.
    pub fn new(config: &TableConfig<'_>, capacity: u64, chunk_bytes: usize) -> Result<Self> {
        ensure!(
            chunk_bytes.is_multiple_of(8) && (CHUNK_HEADER..=MAX_CHUNK_LEN).contains(&chunk_bytes),
            "a chunk of {chunk_bytes} bytes is not a multiple of 8 of {CHUNK_HEADER} to \
             {MAX_CHUNK_LEN}"
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
        Ok(Self {
            index,
            chunks: Vec::new(),
            bases: Vec::new(),
            lens: Vec::new(),
            chunk_bytes,
            linked: Vec::new(),
            capacity,
        })
    }

    /// Empty the table, keeping its index and chunks, for a caller that
    /// builds it again.
    pub fn reset(&mut self) -> Result<()> {
        let kinds = self.table()?.key_kinds().to_vec();
        let payload_size = self.table()?.payload_size();
        let config = TableConfig {
            keys: &kinds,
            payload_size,
        };
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
                init_chunk(block.base(), block.bytes())?;
            }
        }
        self.linked.fill(CHUNK_HEADER);
        Ok(())
    }

    fn chunk_set(&self) -> Chunks<'_> {
        // SAFETY: the owned chunks are valid for their lengths while `self`
        // lives, and only tables over this owner access them.
        unsafe { Chunks::new(&self.bases, &self.lens) }.expect("the owned chunks are valid")
    }

    /// The table, shared.
    pub fn table(&self) -> Result<Table<'_>> {
        // SAFETY: the index and chunks are owned and live as long as the
        // borrow.
        unsafe { Table::attach(self.index.base(), self.index.bytes(), self.chunk_set()) }
    }

    /// The table, for its one writer.
    pub fn table_mut(&mut self) -> Result<TableMut<'_>> {
        // SAFETY: as for `table`, with the owner borrowed exclusively.
        unsafe { TableMut::attach_mut(self.index.base(), self.index.bytes(), self.chunk_set()) }
    }

    /// Add an empty chunk and return its number.
    pub fn add_chunk(&mut self) -> Result<usize> {
        ensure!(
            self.chunks.len() < MAX_CHUNKS,
            "a table has at most {MAX_CHUNKS} chunks"
        );
        let block = Block::new(self.chunk_bytes / 8);
        // SAFETY: the block is new, aligned to 8 and of a checked length.
        unsafe { init_chunk(block.base(), block.bytes()) }?;
        self.bases.push(block.base());
        self.lens.push(block.bytes());
        self.chunks.push(block);
        self.linked.push(CHUNK_HEADER);
        Ok(self.chunks.len() - 1)
    }

    /// The number of chunks.
    pub fn chunks(&self) -> usize {
        self.chunks.len()
    }

    /// Move the table to a new index sized for `capacity` records.
    pub fn regrow(&mut self, capacity: u64) -> Result<()> {
        let config_len = {
            let table = self.table()?;
            let kinds = table.key_kinds().to_vec();
            let payload_size = table.payload_size();
            index_size(
                &TableConfig {
                    keys: &kinds,
                    payload_size,
                },
                capacity,
            )?
        };
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

    /// Append the rows of `pending` to the last chunk, adding chunks as they
    /// fill, and link them: the count inserted is returned, and every row
    /// leaves `pending` with its reference in `offsets`.
    pub fn insert<K: KeySource + ?Sized>(
        &mut self,
        hashes: &[u32],
        keys: &K,
        payload: Option<&[u8]>,
        pending: &mut RowMask<'_>,
        offsets: &mut [u32],
    ) -> Result<usize> {
        let appended = self.append_all(hashes, keys, payload, pending, offsets)?;
        let mut linked = core::mem::take(&mut self.linked);
        let result = (|| {
            let table = self.table()?;
            for (chunk, from) in linked.iter_mut().enumerate() {
                table.link(chunk, from)?;
            }
            Ok::<_, anyhow::Error>(())
        })();
        self.linked = linked;
        result?;
        Ok(appended)
    }

    /// As [`Self::insert`], linking by [`TableMut::link_grouped`]: the
    /// records of a key lie next to each other. Returns the count inserted
    /// and, of them, those whose keys the table held already.
    pub fn insert_grouped<K: KeySource + ?Sized>(
        &mut self,
        hashes: &[u32],
        keys: &K,
        payload: Option<&[u8]>,
        pending: &mut RowMask<'_>,
        offsets: &mut [u32],
    ) -> Result<(usize, usize)> {
        let appended = self.append_all(hashes, keys, payload, pending, offsets)?;
        let mut linked = core::mem::take(&mut self.linked);
        let mut duplicates = 0;
        let result = (|| {
            let mut table = self.table_mut()?;
            for (chunk, from) in linked.iter_mut().enumerate() {
                duplicates += table.link_grouped(chunk, from)?.1;
            }
            Ok::<_, anyhow::Error>(())
        })();
        self.linked = linked;
        result?;
        Ok((appended, duplicates))
    }

    /// Append every row of `pending`, adding chunks as they fill.
    fn append_all<K: KeySource + ?Sized>(
        &mut self,
        hashes: &[u32],
        keys: &K,
        payload: Option<&[u8]>,
        pending: &mut RowMask<'_>,
        offsets: &mut [u32],
    ) -> Result<usize> {
        let mut appended = 0;
        loop {
            if self.chunks.is_empty() {
                self.add_chunk()?;
            }
            let chunk = self.chunks.len() - 1;
            let count = self
                .table()?
                .append(chunk, hashes, keys, payload, pending, offsets)?;
            appended += count;
            if pending.as_view().selected_count() == 0 {
                return Ok(appended);
            }
            ensure!(
                count > 0 || !self.chunk_is_empty(chunk),
                "a record does not fit a chunk of {} bytes",
                self.chunk_bytes
            );
            self.add_chunk()?;
        }
    }

    fn chunk_is_empty(&mut self, chunk: usize) -> bool {
        self.chunks[chunk].words_mut()[0] == CHUNK_HEADER as u64
    }

    /// Give each row of `pending` the record of its keys, as
    /// [`TableMut::find_or_insert`] does, adding chunks and building a
    /// larger index as needed: every row leaves `pending`.
    pub fn find_or_insert<K: KeySource + ?Sized>(
        &mut self,
        hashes: &[u32],
        keys: &K,
        pending: &mut RowMask<'_>,
        offsets: &mut [u32],
        inserted: &mut RowMask<'_>,
    ) -> Result<usize> {
        let nrows = pending.as_view().nrows();
        let mut words = vec![0u64; nrows.div_ceil(64)];
        for index in 0..words.len() {
            inserted.set_word(index, 0)?;
        }
        let mut resolved = 0;
        loop {
            if self.chunks.is_empty() {
                self.add_chunk()?;
            }
            let chunk = self.chunks.len() - 1;
            let mut created = RowMask::try_new(nrows, &mut words)?;
            let count = self.table_mut()?.find_or_insert(
                chunk,
                hashes,
                keys,
                pending,
                offsets,
                &mut created,
            )?;
            resolved += count;
            for (index, word) in words.iter().enumerate() {
                let merged = inserted.as_view().word(index).unwrap() | word;
                inserted.set_word(index, merged)?;
            }
            if pending.as_view().selected_count() == 0 {
                return Ok(resolved);
            }
            let stats = self.table()?.stats();
            if stats.records >= stats.buckets / 2 {
                self.regrow(stats.records * 2)?;
            } else {
                self.add_chunk()?;
            }
            // The rows `inserted` holds were created by earlier rounds.
            words.fill(0);
        }
    }

    /// The index's words, for a test to read or damage.
    pub fn index_words(&mut self) -> &mut [u64] {
        self.index.words_mut()
    }

    /// A chunk's words, for a test to read or damage.
    pub fn chunk_words(&mut self, chunk: usize) -> &mut [u64] {
        self.chunks[chunk].words_mut()
    }
}
