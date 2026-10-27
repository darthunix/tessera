//! What the fuzz targets of relhash share: a reader of the fuzzer's
//! bytes, a valid table built from them in blocks of its own, and the
//! calls of the crate over it, each with arguments the bytes choose. A
//! target passes when no call panics, none runs without end, and none
//! reads or writes outside its blocks, which AddressSanitizer watches.

use core::cell::Cell;

use relhash::{
    Appended, Batch, CHUNK_HEADER, ChunkWriter, Chunks, Cursor, Error, HELD, KeySource, Marks,
    Matches, Partitions, PayloadColumns, RowMask, RowMaskView, Table, TableConfig, TableMut, bloom,
    index_size, mark, mark_words, ones, record_bytes, scan_unmarked,
};

/// Steps of a loop over a call, past which the loop is taken for one
/// without end.
const MOST_STEPS: usize = 100_000;

/// Bytes of payload of a record: one column, after its word of NULL bits.
const PAYLOAD: usize = 16;

/// The fuzzer's bytes, read from the front; past their end, zeros.
pub struct Input<'a>(&'a [u8]);

impl<'a> Input<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self(data)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn byte(&mut self) -> u8 {
        match self.0.split_first() {
            Some((&byte, rest)) => {
                self.0 = rest;
                byte
            }
            None => 0,
        }
    }

    /// A number below `n`, 0 when `n` is 0.
    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { self.u32() as usize % n }
    }

    pub fn u32(&mut self) -> u32 {
        u32::from_le_bytes([self.byte(), self.byte(), self.byte(), self.byte()])
    }

    pub fn u64(&mut self) -> u64 {
        u64::from(self.u32()) | u64::from(self.u32()) << 32
    }
}

/// Rows of one or two keys, the first of which may be NULL.
pub struct Rows {
    nkeys: usize,
    keys: Vec<[i64; 2]>,
    non_null: Vec<u64>,
}

impl KeySource for Rows {
    fn nkeys(&self) -> usize {
        self.nkeys
    }

    fn nrows(&self) -> usize {
        self.keys.len()
    }

    fn word(&self, key: usize, index: usize, selected: u64, out: &mut [i64; 64]) -> Option<u64> {
        let non_null = match key {
            0 => selected & self.non_null.get(index)?,
            _ => selected,
        };
        for bit in ones(selected) {
            let keys = self.keys.get(index * 64 + bit)?;
            out[bit] = if non_null >> bit & 1 == 1 {
                keys[key]
            } else {
                0
            };
        }
        Some(non_null)
    }
}

/// A hash of a key: its high bits pick the bucket.
fn hash(key: i64) -> u32 {
    ((key as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32) as u32
}

/// A block of the table's memory: words, so that it is aligned to 8,
/// reached only through the pointer taken once.
struct Block {
    _words: Vec<u64>,
    base: *mut u8,
    len: usize,
}

impl Block {
    fn new(len: usize) -> Self {
        let mut words = vec![0_u64; len / 8];
        let base = words.as_mut_ptr().cast();
        Self {
            _words: words,
            base,
            len,
        }
    }
}

/// The marks of the chunks, one run of words each.
struct MarkWords(Vec<Vec<Cell<u64>>>);

impl Marks for MarkWords {
    fn set(&self, chunk: usize, word: usize, bit: u64) {
        let word = &self.0[chunk][word];
        word.set(word.get() | bit);
    }

    fn word(&self, chunk: usize, word: usize) -> u64 {
        self.0[chunk][word].get()
    }
}

/// How the table is built.
#[derive(Clone, Copy)]
enum Shape {
    Appended,
    Grouped,
    Regrown,
    Split,
}

/// The spare chunks after the table's: the one its appends link, the
/// source of a merge, two of the calls by partition, two of a chunk
/// writer's and two targets of a split.
const WORK: usize = 0;
const SOURCE: usize = 1;
const PARTS: [usize; 2] = [2, 3];
const WRITER_PARTS: [usize; 2] = [4, 5];
const SPLIT_PARTS: [usize; 2] = [6, 7];
const SPARE: usize = 8;

/// A table, every block its calls take, and the rows it was built of.
pub struct World {
    config: TableConfig,
    record_size: usize,
    capacity: u64,
    index: Block,
    blocks: Vec<Block>,
    bases: Vec<*mut u8>,
    lens: Vec<usize>,
    table_chunks: usize,
    rows: Rows,
    hashes: Vec<u32>,
    payload: Vec<u8>,
    values: Vec<u64>,
    nulls: Vec<bool>,
    /// Every record of the table as built.
    refs: Vec<u32>,
    filter: Vec<u64>,
    shared: Block,
    marks: MarkWords,
    /// Whether the calls take any arguments the bytes give, rather than
    /// those of the table.
    wild: bool,
}

impl World {
    /// A valid table of rows the bytes give, in a shape they choose.
    pub fn build(input: &mut Input<'_>, wild: bool) -> Self {
        let nkeys = 1 + input.below(2);
        let config = TableConfig {
            nkeys,
            payload_size: PAYLOAD,
        };
        let record_size = record_bytes(&config).unwrap();
        let nrows = 1 + input.below(128);
        let mut keys = Vec::with_capacity(nrows);
        let mut non_null = vec![0_u64; nrows.div_ceil(64)];
        for row in 0..nrows {
            let byte = input.byte();
            // Few values, so that keys repeat.
            keys.push([i64::from(byte & 15), i64::from(byte >> 4 & 3)]);
            if byte >> 6 != 0 {
                non_null[row / 64] |= 1 << (row % 64);
            }
        }
        let hashes = keys
            .iter()
            .enumerate()
            .map(|(row, keys)| {
                let first = if non_null[row / 64] >> (row % 64) & 1 == 1 {
                    hash(keys[0])
                } else {
                    0
                };
                if nkeys == 2 {
                    first ^ hash(keys[1]).rotate_left(5)
                } else {
                    first
                }
            })
            .collect();
        let shape = [
            Shape::Appended,
            Shape::Grouped,
            Shape::Regrown,
            Shape::Split,
        ][input.below(4)];
        let room = 1 + input.below(nrows);
        let (table_chunks, table_len) = match shape {
            Shape::Split => (2, CHUNK_HEADER + record_size * nrows),
            _ => (nrows.div_ceil(room), CHUNK_HEADER + record_size * room),
        };
        let spare_len = CHUNK_HEADER + record_size * (1 + input.below(2 * nrows));
        let lens: Vec<usize> = (0..table_chunks)
            .map(|_| table_len)
            .chain((0..SPARE).map(|_| spare_len))
            .chain([CHUNK_HEADER + record_size * nrows])
            .collect();
        let blocks: Vec<Block> = lens.iter().map(|&len| Block::new(len)).collect();
        let bases: Vec<*mut u8> = blocks.iter().map(|block| block.base).collect();
        for block in &blocks {
            // SAFETY: a block of `len` bytes, aligned to 8, ours alone.
            unsafe { Chunks::init(block.base, block.len) }.unwrap();
        }
        let values: Vec<u64> = (0..nrows as u64).collect();
        let nulls: Vec<bool> = (0..nrows).map(|row| row % 3 == 2).collect();
        let payload = values
            .iter()
            .zip(&nulls)
            .flat_map(|(&value, &null)| [u64::from(null), value])
            .flat_map(u64::to_ne_bytes)
            .collect();
        let capacity = match shape {
            Shape::Regrown => 1,
            _ => nrows as u64,
        };
        let mut world = Self {
            config,
            record_size,
            capacity,
            index: Block::new(index_size(&config, capacity).unwrap()),
            blocks,
            bases,
            lens,
            table_chunks,
            rows: Rows {
                nkeys,
                keys,
                non_null,
            },
            hashes,
            payload,
            values,
            nulls,
            refs: Vec::new(),
            filter: Vec::new(),
            shared: Block::new(8),
            marks: MarkWords(Vec::new()),
            wild,
        };
        world.fill(shape);
        world.refs = world.scan_refs();
        let records = world.refs.len() as u64;
        world.filter = vec![0; bloom::words_for(records).unwrap()];
        world.shared = Block::new(8 * bloom::shared_words_for(records).unwrap());
        world.marks = MarkWords(
            world
                .lens
                .iter()
                .map(|&len| vec![Cell::new(0); mark_words(len, record_size).unwrap()])
                .collect(),
        );
        world
    }

    /// Put the rows in the table as `shape` says, then append them to the
    /// merge's source unlinked.
    fn fill(&mut self, shape: Shape) {
        let nrows = self.nrows();
        let config = self.config;
        if let Shape::Split = shape {
            let build = self.lens.len() - 1;
            let writer = self.writer().unwrap();
            let (mut pending, mut refs) = (all_rows(nrows), vec![0; nrows]);
            let mut batch = Batch::new(&self.hashes, &self.rows, &mut pending, &mut refs).unwrap();
            writer
                .append(build, Some(&self.payload), &mut batch)
                .unwrap();
            let partitions = Partitions {
                shift: 31,
                chunks: &[0, 1],
            };
            let (mut from, mut copies, mut copied) = (CHUNK_HEADER, [0; 16], [0; 16]);
            loop {
                let split = writer
                    .split(&partitions, build, &mut from, &mut copies, &mut copied)
                    .unwrap();
                if split.count == 0 {
                    break;
                }
            }
            // SAFETY: the block, which nothing borrows now.
            unsafe { Chunks::init(self.bases[build], self.lens[build]) }.unwrap();
        }
        // SAFETY: the index is ours alone, and no chunk writer lives.
        let mut table = unsafe {
            TableMut::create(
                self.index.base,
                self.index.len,
                &config,
                self.capacity,
                self.chunks(),
            )
        }
        .unwrap();
        let (mut pending, mut refs) = (all_rows(nrows), vec![0; nrows]);
        let mut batch = Batch::new(&self.hashes, &self.rows, &mut pending, &mut refs).unwrap();
        for chunk in 0..self.table_chunks {
            match shape {
                Shape::Appended | Shape::Regrown => {
                    table
                        .append(chunk, Some(&self.payload), &mut batch)
                        .unwrap();
                }
                Shape::Grouped => {
                    let mut inserted = vec![0; nrows.div_ceil(64)];
                    let mut inserted = RowMask::try_new(nrows, &mut inserted).unwrap();
                    table
                        .find_or_insert(chunk, &mut batch, &mut inserted)
                        .unwrap();
                    continue;
                }
                Shape::Split => {}
            }
            let mut from = CHUNK_HEADER;
            table.link(chunk, &mut from).unwrap();
        }
        if let Shape::Regrown = shape {
            let capacity = 4 * nrows as u64;
            let grown = Block::new(index_size(&config, capacity).unwrap());
            // SAFETY: the new index is ours alone.
            unsafe { table.regrow(grown.base, grown.len, capacity) }.unwrap();
            self.index = grown;
            self.capacity = capacity;
        }
        let (mut pending, mut refs) = (all_rows(nrows), vec![0; nrows]);
        let mut batch = Batch::new(&self.hashes, &self.rows, &mut pending, &mut refs).unwrap();
        let source = self.spare(SOURCE);
        self.writer()
            .unwrap()
            .append(source, Some(&self.payload), &mut batch)
            .unwrap();
    }

    /// Every record of the chunks, linked or not.
    fn scan_refs(&self) -> Vec<u32> {
        let table = self.table_mut().unwrap();
        let (mut cursor, mut out, mut refs) = (Cursor::start(), [0; 64], Vec::new());
        loop {
            let count = table.scan(&mut cursor, &mut out).unwrap();
            if count == 0 {
                return refs;
            }
            refs.extend_from_slice(&out[..count]);
        }
    }

    /// Write bytes the input gives at places it chooses, in the index or
    /// a chunk, `count` times.
    pub fn damage(&mut self, input: &mut Input<'_>, count: usize) {
        for _ in 0..count {
            let block = input.below(1 + self.blocks.len());
            let (base, len) = match block {
                0 => (self.index.base, self.index.len),
                _ => (self.bases[block - 1], self.lens[block - 1]),
            };
            let at = input.below(len);
            let width = (1 + input.below(8)).min(len - at);
            for byte in at..at + width {
                // SAFETY: a byte of the block, which no handle borrows.
                unsafe { base.add(byte).write(input.byte()) };
            }
        }
    }

    fn spare(&self, spare: usize) -> usize {
        self.table_chunks + spare
    }

    fn nrows(&self) -> usize {
        self.hashes.len()
    }

    fn chunks(&self) -> Chunks<'_> {
        // SAFETY: the blocks live as long as the world, each in a vector
        // of its own, and are reached only through one handle at a time.
        unsafe { Chunks::new(&self.bases, &self.lens) }.unwrap()
    }

    fn table(&self) -> Result<Table<'_>, Error> {
        // SAFETY: as `chunks`; no table writes while this one lives.
        unsafe { Table::attach(self.index.base, self.index.len, self.chunks()) }
    }

    fn table_mut(&self) -> Result<TableMut<'_>, Error> {
        // SAFETY: as `chunks`; this handle is the one that lives.
        unsafe { TableMut::attach_mut(self.index.base, self.index.len, self.chunks()) }
    }

    fn writer(&self) -> Result<ChunkWriter<'_>, Error> {
        // SAFETY: as `chunks`; no table lives beside the writer.
        unsafe { ChunkWriter::new(&self.config, self.chunks()) }
    }

    /// Call `call` with the rows' column of payload.
    fn with_columns<T>(&self, call: impl FnOnce(&PayloadColumns<'_>) -> T) -> T {
        let (values, nulls) = ([self.values.as_slice()], [self.nulls.as_slice()]);
        call(&PayloadColumns::new(&values, &nulls, self.nrows()).unwrap())
    }

    /// A reference: one of the table's, or when the calls are wild, any
    /// the bytes give, some of the time.
    fn reference(&self, input: &mut Input<'_>) -> u32 {
        if self.refs.is_empty() || (self.wild && input.byte() < 64) {
            input.u32()
        } else {
            self.refs[input.below(self.refs.len())]
        }
    }

    /// A number of a chunk: `chunk`, or when the calls are wild, any
    /// chunk or one past them, some of the time.
    fn chunk(&self, input: &mut Input<'_>, chunk: usize) -> usize {
        if self.wild && input.byte() < 64 {
            input.below(self.lens.len() + 2)
        } else {
            chunk
        }
    }

    /// A byte of a chunk where a walk starts: `from`, or when the calls
    /// are wild, any the bytes give, some of the time.
    fn from(&self, input: &mut Input<'_>, from: usize) -> usize {
        if self.wild && input.byte() < 64 {
            input.u32() as usize
        } else {
            from
        }
    }

    /// The rows of a call: every row, or those the bytes pick.
    fn rows(&self, input: &mut Input<'_>) -> Vec<u64> {
        let mut rows = all_rows(self.nrows());
        if input.byte() & 1 == 1 {
            let last = rows.len() - 1;
            for word in &mut rows {
                *word &= input.u64();
            }
            // Bits past the last row are the mask's padding, which must
            // be clear.
            rows[last] &= all_rows(self.nrows())[last];
        }
        rows
    }

    /// The partitions of a call over chunks `chunks`.
    fn partitions(&self, input: &mut Input<'_>, chunks: [usize; 2]) -> ([u32; 2], u32) {
        let shift = if self.wild && input.byte() < 64 {
            input.u32()
        } else {
            31
        };
        (
            chunks.map(|chunk| self.chunk(input, self.spare(chunk)) as u32),
            shift,
        )
    }

    /// Run the call the bytes choose.
    pub fn call(&mut self, input: &mut Input<'_>) {
        let _ = self.try_call(input);
    }

    fn try_call(&mut self, input: &mut Input<'_>) -> Result<(), Error> {
        let nrows = self.nrows();
        match input.below(20) {
            0 => {
                let table = self.table()?;
                let _ = table.stats();
                table.check_config(&self.config)?;
            }
            1 | 2 => {
                let table = self.table()?;
                let mut found = self.rows(input);
                let mut refs = vec![0; nrows];
                let mut matches = Matches::new(&mut found, &mut refs)?;
                table.probe(&self.hashes, &self.rows, &mut matches)?;
                let group = input.byte() & 1 == 1;
                drain(|| match group {
                    true => table.next_in_group(&mut matches),
                    false => table.next_match(&mut matches),
                })?;
            }
            3 => {
                // Matches the caller made up, as a wild caller may.
                let table = self.table()?;
                let mut found = self.rows(input);
                let mut refs: Vec<u32> = (0..nrows).map(|_| self.reference(input)).collect();
                let mut matches = Matches::new(&mut found, &mut refs)?.with_steps(input.u64());
                drain(|| table.next_match(&mut matches))?;
            }
            4 => {
                let table = self.table()?;
                let reference = self.reference(input);
                table.record(reference)?;
                table.records().record(self.reference(input))?;
            }
            5 => {
                let table = self.table_mut()?;
                let (mut cursor, mut out) = (Cursor::start(), vec![0; 1 + input.below(70)]);
                drain(|| table.scan(&mut cursor, &mut out))?;
            }
            6 => {
                let table = self.table()?;
                let (mut cursor, mut out) = (Cursor::start(), vec![0; 1 + input.below(70)]);
                drain(|| table.scan_unmarked(&mut cursor, Some(&self.marks), &mut out))?;
                let (mut cursor, chunks) = (Cursor::start(), self.chunks());
                drain(|| {
                    scan_unmarked(
                        &chunks,
                        self.record_size,
                        &mut cursor,
                        Some(&self.marks),
                        &mut out,
                    )
                })?;
            }
            7 => {
                let table = self.table()?;
                let refs: Vec<u32> = (0..nrows).map(|_| self.reference(input)).collect();
                let rows = self.rows(input);
                let rows = RowMaskView::try_new(nrows, &rows)?;
                table.mark(&refs, &rows, &self.marks)?;
                mark(&self.lens, self.record_size, &refs, &rows, &self.marks)?;
            }
            8 => {
                self.chunks().check_all()?;
                for (&base, &len) in self.bases.iter().zip(&self.lens) {
                    // SAFETY: a chunk's block, which nothing writes meanwhile.
                    let _ = unsafe { Chunks::used(base, len, self.record_size) };
                }
            }
            9 => {
                let mut filter = core::mem::take(&mut self.filter);
                let built = self.table().and_then(|table| table.bloom(&mut filter));
                if built.is_ok() {
                    let mut rows = self.rows(input);
                    let mut rows = RowMask::try_new(nrows, &mut rows)?;
                    let _ = bloom::probe(&filter, &self.hashes, &mut rows);
                }
                self.filter = filter;
                built?;
            }
            10 => {
                // SAFETY: the block's words, reached only through this filter.
                let filter = unsafe {
                    bloom::SharedFilter::attach(self.shared.base.cast(), self.shared.len / 8)
                }?;
                filter.init();
                self.table()?.try_build_bloom(&filter)?;
                let mut rows = self.rows(input);
                let mut rows = RowMask::try_new(nrows, &mut rows)?;
                bloom::probe_shared(&filter, &self.hashes, &mut rows)?;
            }
            11 => {
                let mut table = self.table_mut()?;
                let slot = input.below(HELD + 1);
                let mut cursor = table.payloads();
                let reference = self.reference(input);
                cursor.payload(reference)?;
                cursor.hold(slot, self.reference(input))?;
                cursor.held(slot)?;
                table.payload_mut(self.reference(input))?;
                let refs: Vec<u32> = (0..nrows).map(|_| self.reference(input)).collect();
                let rows = self.rows(input);
                let rows = RowMaskView::try_new(nrows, &rows)?;
                let mut out = vec![core::ptr::null_mut(); nrows];
                table.payload_ptrs(&refs, &rows, &mut out)?;
            }
            12 | 13 => {
                let mut table = self.table_mut()?;
                let mut pending = self.rows(input);
                let mut refs = vec![0; nrows];
                let mut batch = Batch::new(&self.hashes, &self.rows, &mut pending, &mut refs)?;
                let mut inserted = vec![0; nrows.div_ceil(64)];
                let mut inserted = RowMask::try_new(nrows, &mut inserted)?;
                if input.byte() & 1 == 0 {
                    let chunk = self.chunk(input, self.spare(WORK));
                    table.find_or_insert(chunk, &mut batch, &mut inserted)?;
                } else {
                    let (chunks, shift) = self.partitions(input, PARTS);
                    let partitions = Partitions {
                        shift,
                        chunks: &chunks,
                    };
                    table.find_or_insert_partitioned(&partitions, &mut batch, &mut inserted)?;
                }
            }
            14 | 15 => {
                let chunk = self.chunk(input, self.spare(WORK));
                let mut table = self.table_mut()?;
                let used = (self.bases.get(chunk).zip(self.lens.get(chunk)))
                    // SAFETY: a chunk's block, which the table only reads
                    // meanwhile.
                    .and_then(|(&base, &len)| {
                        unsafe { Chunks::used(base, len, self.record_size) }.ok()
                    })
                    .unwrap_or(CHUNK_HEADER);
                let mut from = self.from(input, used);
                let mut pending = self.rows(input);
                let mut refs = vec![0; nrows];
                let mut batch = Batch::new(&self.hashes, &self.rows, &mut pending, &mut refs)?;
                if input.byte() & 1 == 0 {
                    table.append(chunk, Some(&self.payload), &mut batch)?;
                } else {
                    self.with_columns(|columns| table.append_columns(chunk, columns, &mut batch))?;
                }
                match input.below(3) {
                    0 => {
                        table.link(chunk, &mut from)?;
                    }
                    1 => {
                        table.link_counting(chunk, &mut from)?;
                    }
                    _ => {
                        table.link_grouped(chunk, &mut from)?;
                    }
                }
            }
            16 => {
                let mut table = self.table_mut()?;
                let source = self.chunk(input, self.spare(SOURCE));
                let chunk = self.chunk(input, self.spare(WORK));
                let mut from = self.from(input, CHUNK_HEADER);
                table.merge(source, &mut from, chunk, |incoming, ours| {
                    ours.copy_from_slice(incoming);
                    Ok::<_, Error>(())
                })?;
            }
            17 => {
                let writer = self.writer()?;
                let (chunks, shift) = self.partitions(input, WRITER_PARTS);
                let partitions = Partitions {
                    shift,
                    chunks: &chunks,
                };
                let mut pending = self.rows(input);
                let mut refs = vec![0; nrows];
                let mut batch = Batch::new(&self.hashes, &self.rows, &mut pending, &mut refs)?;
                let (mut counts, mut nulls) = ([0; 2], 0);
                match input.below(3) {
                    0 => {
                        self.with_columns(|columns| {
                            writer.append_partitioned_columns(
                                &partitions,
                                columns,
                                &mut batch,
                                &mut Appended {
                                    rows: &mut counts,
                                    nulls: &mut nulls,
                                },
                            )
                        })?;
                    }
                    1 => {
                        writer.append(chunks[0] as usize, Some(&self.payload), &mut batch)?;
                    }
                    _ => {
                        self.with_columns(|columns| {
                            writer.append_columns(chunks[1] as usize, columns, &mut batch)
                        })?;
                    }
                }
            }
            18 => {
                let writer = self.writer()?;
                let (chunks, shift) = self.partitions(input, SPLIT_PARTS);
                let partitions = Partitions {
                    shift,
                    chunks: &chunks,
                };
                let own = input.below(self.table_chunks);
                let source = self.chunk(input, own);
                let mut from = self.from(input, CHUNK_HEADER);
                let (mut copies, mut hashes) = (vec![0; 1 + input.below(20)], vec![0; 20]);
                let ncopies = copies.len();
                drain(|| {
                    let split = writer.split(
                        &partitions,
                        source,
                        &mut from,
                        &mut copies,
                        &mut hashes[..ncopies],
                    )?;
                    Ok(if split.full.is_some() { 0 } else { split.count })
                })?;
            }
            _ => {
                if input.byte() & 1 == 0 {
                    let key = if self.wild { input.below(4) } else { 0 };
                    self.table_mut()?.clear_key(key)?;
                } else {
                    // At most 2^16 records: the index stays small whatever the
                    // count of regrows.
                    let capacity = (self.capacity * (1 + input.below(3) as u64)).min(1 << 16);
                    let grown = Block::new(index_size(&self.config, capacity)?);
                    // SAFETY: the new index is ours alone.
                    unsafe { self.table_mut()?.regrow(grown.base, grown.len, capacity) }?;
                    self.index = grown;
                    self.capacity = capacity;
                }
            }
        }
        Ok(())
    }
}

/// The words of a mask of every one of `nrows` rows.
fn all_rows(nrows: usize) -> Vec<u64> {
    let mut words = vec![u64::MAX; nrows.div_ceil(64)];
    if !nrows.is_multiple_of(64) {
        *words.last_mut().unwrap() = (1 << (nrows % 64)) - 1;
    }
    words
}

/// Run `step` until it gives 0, as a caller's loop over a call does.
fn drain(mut step: impl FnMut() -> Result<usize, Error>) -> Result<(), Error> {
    for _ in 0..MOST_STEPS {
        if step()? == 0 {
            return Ok(());
        }
    }
    panic!("a loop over a call without end");
}
