//! The matrix of damage: valid tables of four shapes, each damaged in one
//! place from a closed list, every field at and around its limits, and
//! every call of the crate run over it twice, so that calls also follow
//! the ones that failed. Each call ends with a result or the error of
//! the rule the damage breaks, a damage that is more than data is seen by
//! some call, and the words around every block stay as they were.

use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};

use super::*;
use crate::table::testing::{Column, Keys};

/// Words of [`PATTERN`] before and after every block.
const GUARD: usize = 4;

/// What the words around a block hold.
const PATTERN: u64 = 0x5a5a_a5a5_0ff0_f00f;

/// The offset of the payload's size in the header, beside the others of
/// `tests.rs`.
const PAYLOAD_SIZE: usize = 60;

/// Bytes of payload of a record: one column, after its word of NULL bits.
const PAYLOAD: usize = 16;

/// The shift of the partitions of the calls by partition: two, by the
/// top bit of the hash.
const SHIFT: u32 = 31;

/// Steps of a caller's loop over a call, past which the loop is taken
/// for one without end.
const MOST_STEPS: usize = 100_000;

/// A block between guard words, read and written only through the
/// pointer taken once from its vector.
struct Guarded {
    _words: Vec<u64>,
    start: *mut u64,
    len: usize,
}

impl Guarded {
    /// A block of `len` bytes of zeros, a multiple of 8.
    fn new(len: usize) -> Self {
        let mut words = vec![PATTERN; len / 8 + 2 * GUARD];
        let start = words.as_mut_ptr();
        // SAFETY: the block's words lie after the guard, in the vector.
        unsafe { start.add(GUARD).write_bytes(0, len / 8) };
        Self {
            _words: words,
            start,
            len,
        }
    }

    fn base(&self) -> *mut u8 {
        // SAFETY: the block starts after the guard, in the vector.
        unsafe { self.start.add(GUARD).cast() }
    }

    /// The value at `byte` of the block.
    fn read<T: Copy>(&self, byte: usize) -> T {
        assert!(byte + size_of::<T>() <= self.len, "a read past the block");
        // SAFETY: inside the block, just checked.
        unsafe { self.base().add(byte).cast::<T>().read_unaligned() }
    }

    /// Write `value` at `byte` of the block.
    fn write<T: Copy>(&self, byte: usize, value: T) {
        assert!(byte + size_of::<T>() <= self.len, "a write past the block");
        // SAFETY: inside the block, just checked.
        unsafe { self.base().add(byte).cast::<T>().write_unaligned(value) }
    }

    /// The block's words, for a call that takes them as a slice.
    fn words(&mut self) -> &mut [u64] {
        // SAFETY: the block's words, borrowed as `self` is, and reached
        // only through the slice while it lives.
        unsafe { core::slice::from_raw_parts_mut(self.base().cast(), self.len / 8) }
    }

    /// Whether the words around the block are as they were made.
    fn guarded(&self) -> bool {
        let words = self.len / 8;
        (0..GUARD)
            .chain(GUARD + words..2 * GUARD + words)
            // SAFETY: the guard words lie in the vector.
            .all(|word| unsafe { self.start.add(word).read() } == PATTERN)
    }
}

/// A write past either end of a block changes its guard words.
#[test]
fn a_write_past_a_block_is_seen() {
    for word in [-1, 2] {
        let block = Guarded::new(16);
        assert!(block.guarded());
        // SAFETY: a guard word, in the vector, just outside the block.
        unsafe { block.base().cast::<u64>().offset(word).write(0) };
        assert!(!block.guarded(), "a write at word {word}");
    }
}

/// How the table of a case is built.
#[derive(Clone, Copy, Debug)]
enum Shape {
    /// Rows appended to three chunks in turn, then linked: a key's
    /// duplicates chain one after another.
    Appended,
    /// Rows found or inserted: a record for each key and its NULLs.
    Grouped,
    /// As `Appended`, in an index of one record's capacity, then moved to
    /// a larger one.
    Regrown,
    /// Rows appended to a chunk alone, split into the chunks of two
    /// partitions, then linked.
    Split,
}

/// A table to damage: its shape, its key count, and its rows, each the
/// values of two keys and whether the first is NULL.
#[derive(Clone, Debug)]
struct Case {
    shape: Shape,
    nkeys: usize,
    rows: Vec<(i32, i32, bool)>,
}

fn cases() -> impl Strategy<Value = Case> {
    let most = if cfg!(miri) { 6 } else { 150 };
    (
        prop_oneof![
            Just(Shape::Appended),
            Just(Shape::Grouped),
            Just(Shape::Regrown),
            Just(Shape::Split)
        ],
        1..=2_usize,
        // Few values, so that keys repeat.
        proptest::collection::vec(
            (0..8_i32, 0..3_i32, proptest::bool::weighted(0.2)),
            1..=most,
        ),
    )
        .prop_map(|(shape, nkeys, rows)| Case { shape, nkeys, rows })
}

/// A table and every block its calls take, with the rows it was built of.
struct World {
    config: TableConfig,
    record_size: usize,
    capacity: u64,
    index: Guarded,
    blocks: Vec<Guarded>,
    bases: Vec<*mut u8>,
    lens: Vec<usize>,
    /// The chunks of the table's records; the spare ones follow.
    table_chunks: usize,
    columns: Vec<Column>,
    hashes: Vec<u32>,
    payload: Vec<u8>,
    values: Vec<u64>,
    nulls: Vec<bool>,
    /// Every record of the table as built, and of the merge's source.
    refs: Vec<u32>,
    /// The first record of the first chunk that holds one.
    victim: u32,
    filter: Guarded,
    shared: Guarded,
    marks: Vec<Guarded>,
}

/// The spare chunks after the table's: the one its appends link, the
/// source of a merge, those of the calls by partition and of a chunk
/// writer's, and the targets of a split.
const WORK: usize = 0;
const SOURCE: usize = 1;
const PARTS: [usize; 2] = [2, 3];
const WRITER_PARTS: [usize; 2] = [4, 5];
const SPLIT_PARTS: [usize; 2] = [6, 7];
/// The chunk a split shape's rows are appended to before their split.
const BUILD: usize = 8;
const SPARE: usize = 9;

impl World {
    fn build(case: &Case) -> Self {
        let config = TableConfig {
            nkeys: case.nkeys,
            payload_size: PAYLOAD,
        };
        let record_size = record_bytes(&config).unwrap();
        let nrows = case.rows.len();
        let (table_chunks, table_len) = match case.shape {
            // Each partition's chunk has room for every row.
            Shape::Split => (2, CHUNK_HEADER + record_size * nrows),
            _ => (3, CHUNK_HEADER + record_size * nrows.div_ceil(3)),
        };
        // Room for half the rows and one: the calls fill these chunks up
        // to their ends, and the build's split chunk holds every row.
        let spare_len = CHUNK_HEADER + record_size * (nrows / 2 + 1);
        let lens: Vec<usize> = (0..table_chunks)
            .map(|_| table_len)
            .chain((0..SPARE).map(|spare| match spare {
                BUILD => CHUNK_HEADER + record_size * nrows,
                _ => spare_len,
            }))
            .collect();
        let blocks: Vec<Guarded> = lens.iter().map(|&len| Guarded::new(len)).collect();
        let bases: Vec<*mut u8> = blocks.iter().map(Guarded::base).collect();
        for (&base, &len) in bases.iter().zip(&lens) {
            // SAFETY: a block of `len` bytes, aligned to 8, ours alone.
            unsafe { Chunks::init(base, len) }.unwrap();
        }
        let firsts: Vec<i32> = case.rows.iter().map(|row| row.0).collect();
        let seconds: Vec<i32> = case.rows.iter().map(|row| row.1).collect();
        let non_null = words(&case.rows.iter().map(|row| !row.2).collect::<Vec<_>>());
        let mut columns = vec![Column::new(&firsts, Some(&non_null))];
        if case.nkeys == 2 {
            columns.push(Column::new(&seconds, None));
        }
        let hashes = case
            .rows
            .iter()
            .map(|&(first, second, null)| {
                let hash = if null { 0 } else { hash_i32(first) };
                if case.nkeys == 2 {
                    hash_combine(hash, hash_i32(second))
                } else {
                    hash
                }
            })
            .collect();
        let values: Vec<u64> = (0..nrows as u64).map(|row| row * 10).collect();
        let nulls: Vec<bool> = (0..nrows).map(|row| row % 5 == 4).collect();
        let payload = values
            .iter()
            .zip(&nulls)
            .flat_map(|(&value, &null)| [u64::from(null), value])
            .flat_map(u64::to_ne_bytes)
            .collect();
        let capacity = match case.shape {
            Shape::Regrown => 1,
            _ => nrows as u64,
        };
        let index = Guarded::new(index_size(&config, capacity).unwrap());
        let mut world = Self {
            config,
            record_size,
            capacity,
            index,
            blocks,
            bases,
            lens,
            table_chunks,
            columns,
            hashes,
            payload,
            values,
            nulls,
            refs: Vec::new(),
            victim: 0,
            filter: Guarded::new(8),
            shared: Guarded::new(8),
            marks: Vec::new(),
        };
        world.fill(case.shape);
        world.refs = world.scan_refs();
        let records = world.refs.len() as u64;
        world.filter = Guarded::new(8 * bloom::words_for(records).unwrap());
        world.shared = Guarded::new(8 * bloom::shared_words_for(records).unwrap());
        world.marks = world
            .lens
            .iter()
            .map(|&len| Guarded::new(8 * mark_words(len, record_size).unwrap()))
            .collect();
        world.victim = (0..world.table_chunks)
            .find(|&chunk| world.used(chunk) > CHUNK_HEADER)
            .map(|chunk| reference(chunk, CHUNK_HEADER))
            .unwrap();
        world
    }

    /// Put the rows in the table as `shape` says, then append them to the
    /// merge's source unlinked.
    fn fill(&mut self, shape: Shape) {
        let nrows = self.nrows();
        let config = self.config;
        let keys = Keys(&self.columns);
        let batch = |pending: &mut Vec<u64>, refs: &mut Vec<u32>| {
            *pending = all_rows(nrows);
            *refs = vec![0; nrows];
        };
        let (mut pending, mut refs) = (Vec::new(), Vec::new());
        // A chunk writer's appends and split come before the table and
        // after it: no writer lives beside it.
        {
            let writer = writer(&config, self.chunks()).unwrap();
            if let Shape::Split = shape {
                let source = self.spare(BUILD);
                batch(&mut pending, &mut refs);
                let mut rows = Batch::new(&self.hashes, &keys, &mut pending, &mut refs).unwrap();
                writer
                    .append(source, Some(&self.payload), &mut rows)
                    .unwrap();
                let partitions = Partitions {
                    shift: SHIFT,
                    chunks: &[0, 1],
                };
                let (mut from, mut copies, mut copied) = (CHUNK_HEADER, [0; 16], [0; 16]);
                loop {
                    let split = writer
                        .split(&partitions, source, &mut from, &mut copies, &mut copied)
                        .unwrap();
                    assert_eq!(split.full, None, "a partition's chunk holds every row");
                    if split.count == 0 {
                        break;
                    }
                }
                // SAFETY: the source block, which nothing borrows now.
                unsafe { Chunks::init(self.bases[source], self.lens[source]) }.unwrap();
            }
        }
        // SAFETY: the index is ours alone, and no chunk writer lives.
        let mut table = unsafe {
            TableMut::create(
                self.index.base(),
                self.index.len,
                &config,
                self.capacity,
                self.chunks(),
            )
        }
        .unwrap();
        batch(&mut pending, &mut refs);
        let mut rows = Batch::new(&self.hashes, &keys, &mut pending, &mut refs).unwrap();
        for chunk in 0..self.table_chunks {
            match shape {
                Shape::Appended | Shape::Regrown => {
                    table.append(chunk, Some(&self.payload), &mut rows).unwrap();
                }
                Shape::Grouped => {
                    let mut inserted = vec![0; nrows.div_ceil(64)];
                    let mut inserted = RowMask::try_new(nrows, &mut inserted).unwrap();
                    table
                        .find_or_insert(chunk, &mut rows, &mut inserted)
                        .unwrap();
                    continue;
                }
                Shape::Split => {}
            }
            let mut from = CHUNK_HEADER;
            table.link(chunk, &mut from).unwrap();
        }
        if !matches!(shape, Shape::Split) {
            assert_eq!(rows.pending().selected_count(), 0, "every row went in");
        }
        if let Shape::Regrown = shape {
            let capacity = 4 * nrows as u64;
            let grown = Guarded::new(index_size(&config, capacity).unwrap());
            // SAFETY: the new index is ours alone.
            unsafe { table.regrow(grown.base(), grown.len, capacity) }.unwrap();
            self.index = grown;
            self.capacity = capacity;
        }
        batch(&mut pending, &mut refs);
        let mut rows = Batch::new(&self.hashes, &keys, &mut pending, &mut refs).unwrap();
        writer(&config, self.chunks())
            .unwrap()
            .append(self.spare(SOURCE), Some(&self.payload), &mut rows)
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

    /// The number of spare chunk `spare`.
    fn spare(&self, spare: usize) -> usize {
        self.table_chunks + spare
    }

    fn chunks(&self) -> Chunks<'_> {
        // SAFETY: the blocks live as long as the world, each in a vector
        // of its own, and are reached only through tables, chunk writers
        // and the damage, one at a time.
        unsafe { Chunks::new(&self.bases, &self.lens) }.unwrap()
    }

    fn table(&self) -> Result<Table<'_>, Error> {
        // SAFETY: as `chunks`; no table writes while this one lives.
        unsafe { Table::attach(self.index.base(), self.index.len, self.chunks()) }
    }

    fn table_mut(&self) -> Result<TableMut<'_>, Error> {
        // SAFETY: as `chunks`; this handle is the one that lives.
        unsafe { TableMut::attach_mut(self.index.base(), self.index.len, self.chunks()) }
    }

    fn keys(&self) -> Keys<'_> {
        Keys(&self.columns)
    }

    fn nrows(&self) -> usize {
        self.hashes.len()
    }

    /// The used mark of chunk `chunk`.
    fn used(&self, chunk: usize) -> usize {
        self.blocks[chunk].read::<u64>(0) as usize
    }

    /// The block and byte of the victim's record.
    fn victim(&self) -> (&Guarded, usize) {
        let (chunk, byte) = placement(self.victim);
        (&self.blocks[chunk], byte)
    }

    /// The byte of the index where the bucket of the victim's hash lies.
    fn victim_bucket(&self) -> usize {
        let (block, byte) = self.victim();
        let hash = block.read::<u32>(byte);
        let shift = self.index.read::<u32>(BUCKET_SHIFT);
        let offset = self.index.read::<u64>(BUCKETS_OFFSET) as usize;
        offset + 4 * (hash >> shift) as usize
    }

    /// Whether the words around every block are as they were made.
    fn guarded(&self) -> bool {
        self.index.guarded()
            && self.blocks.iter().all(Guarded::guarded)
            && self.filter.guarded()
            && self.shared.guarded()
            && self.marks.iter().all(Guarded::guarded)
    }

    /// Call `call` with the rows' column of payload.
    fn with_columns<T>(&self, call: impl FnOnce(&PayloadColumns<'_>) -> T) -> T {
        let (values, nulls) = ([self.values.as_slice()], [self.nulls.as_slice()]);
        call(&PayloadColumns::new(&values, &nulls, self.nrows()).unwrap())
    }
}

/// The marks of the world's chunks.
struct MarkBlocks<'a>(&'a [Guarded]);

impl Marks for MarkBlocks<'_> {
    fn set(&self, chunk: usize, word: usize, bit: u64) {
        let block = &self.0[chunk];
        block.write(8 * word, block.read::<u64>(8 * word) | bit);
    }

    fn word(&self, chunk: usize, word: usize) -> u64 {
        self.0[chunk].read(8 * word)
    }
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

/// Probe the rows of `rows`, then step each to its further matches with
/// `next`.
fn probe(
    world: &World,
    rows: impl Fn(usize) -> bool,
    next: fn(&Table<'_>, &mut Matches<'_>) -> Result<usize, Error>,
) -> Result<(), Error> {
    let nrows = world.nrows();
    let table = world.table()?;
    let mut found = words(&(0..nrows).map(rows).collect::<Vec<_>>());
    let mut refs = vec![0; nrows];
    let mut matches = Matches::new(&mut found, &mut refs)?;
    if table.probe(&world.hashes, &world.keys(), &mut matches)? == 0 {
        return Ok(());
    }
    drain(|| next(&table, &mut matches))
}

/// Append every row to the chunk the calls link, from where its records
/// end, and link them with `link`.
fn append_and_link(
    world: &World,
    columns: bool,
    link: fn(&mut TableMut<'_>, usize, &mut usize) -> Result<usize, Error>,
) -> Result<(), Error> {
    let chunk = world.spare(WORK);
    let mut table = world.table_mut()?;
    // SAFETY: the chunk's block, which the table only reads meanwhile.
    let mut from =
        unsafe { Chunks::used(world.bases[chunk], world.lens[chunk], world.record_size) }?;
    let (mut pending, mut refs) = (all_rows(world.nrows()), vec![0; world.nrows()]);
    let keys = world.keys();
    let mut batch = Batch::new(&world.hashes, &keys, &mut pending, &mut refs)?;
    if columns {
        world.with_columns(|columns| table.append_columns(chunk, columns, &mut batch))?;
    } else {
        table.append(chunk, Some(&world.payload), &mut batch)?;
    }
    link(&mut table, chunk, &mut from).map(drop)
}

/// A call of the crate over a world, by its name.
type Call = (&'static str, fn(&mut World) -> Result<(), Error>);

/// Every call of the crate that reads a table's blocks.
const CALLS: &[Call] = &[
    ("attach", |world| {
        let table = world.table()?;
        let _ = table.stats();
        table.check_config(&world.config)
    }),
    ("attach_mut", |world| world.table_mut().map(drop)),
    ("probe by words", |world| {
        probe(world, |_| true, |table, matches| table.next_match(matches))
    }),
    ("probe by rows", |world| {
        probe(
            world,
            // Fewer rows of a word than go by whole words.
            |row| row % 16 == 0,
            |table, matches| table.next_match(matches),
        )
    }),
    ("probe by groups", |world| {
        probe(
            world,
            |_| true,
            |table, matches| table.next_in_group(matches),
        )
    }),
    ("record", |world| {
        let table = world.table()?;
        let mut records = table.records();
        for &reference in &world.refs {
            table.record(reference)?;
            records.record(reference)?;
        }
        Ok(())
    }),
    ("scan", |world| {
        let table = world.table_mut()?;
        let (mut cursor, mut out) = (Cursor::start(), [0; 7]);
        drain(|| table.scan(&mut cursor, &mut out))
    }),
    ("scan unmarked", |world| {
        let table = world.table()?;
        let marks = MarkBlocks(&world.marks);
        let (mut cursor, mut out) = (Cursor::start(), [0; 7]);
        drain(|| table.scan_unmarked(&mut cursor, Some(&marks), &mut out))?;
        let (mut cursor, chunks) = (Cursor::start(), world.chunks());
        drain(|| {
            scan_unmarked(
                &chunks,
                world.record_size,
                &mut cursor,
                Some(&marks),
                &mut out,
            )
        })
    }),
    ("mark", |world| {
        let table = world.table()?;
        let rows = all_rows(world.refs.len());
        let rows = RowMaskView::try_new(world.refs.len(), &rows)?;
        let marks = MarkBlocks(&world.marks);
        table.mark(&world.refs, &rows, &marks)?;
        mark(&world.lens, world.record_size, &world.refs, &rows, &marks)
    }),
    ("used marks", |world| {
        world.chunks().check_all()?;
        for (&base, &len) in world.bases.iter().zip(&world.lens) {
            // SAFETY: a chunk's block, which nothing writes meanwhile.
            unsafe { Chunks::used(base, len, world.record_size) }?;
        }
        Ok(())
    }),
    ("bloom", |world| {
        let mut filter = core::mem::replace(&mut world.filter, Guarded::new(8));
        let result = (|| {
            world.table()?.bloom(filter.words())?;
            let mut rows = all_rows(world.nrows());
            let mut rows = RowMask::try_new(world.nrows(), &mut rows)?;
            bloom::probe(filter.words(), &world.hashes, &mut rows).map(drop)
        })();
        world.filter = filter;
        result
    }),
    ("shared bloom", |world| {
        let nwords = world.shared.len / 8;
        // SAFETY: the block's words, reached only through this filter.
        let filter = unsafe { bloom::SharedFilter::attach(world.shared.base().cast(), nwords) }?;
        filter.init();
        world.table()?.try_build_bloom(&filter)?;
        let mut rows = all_rows(world.nrows());
        let mut rows = RowMask::try_new(world.nrows(), &mut rows)?;
        bloom::probe_shared(&filter, &world.hashes, &mut rows).map(drop)
    }),
    ("payloads", |world| {
        let mut table = world.table_mut()?;
        let mut cursor = table.payloads();
        for &reference in &world.refs {
            cursor.payload(reference)?;
            cursor.hold(0, reference)?;
            cursor.held(0)?;
        }
        for &reference in &world.refs {
            table.payload_mut(reference)?;
        }
        let rows = all_rows(world.refs.len());
        let rows = RowMaskView::try_new(world.refs.len(), &rows)?;
        let mut out = vec![core::ptr::null_mut(); world.refs.len()];
        table.payload_ptrs(&world.refs, &rows, &mut out)
    }),
    ("find or insert", |world| {
        let nrows = world.nrows();
        let mut table = world.table_mut()?;
        let (mut pending, mut refs) = (all_rows(nrows), vec![0; nrows]);
        let keys = world.keys();
        let mut batch = Batch::new(&world.hashes, &keys, &mut pending, &mut refs)?;
        let mut inserted = vec![0; nrows.div_ceil(64)];
        let mut inserted = RowMask::try_new(nrows, &mut inserted)?;
        table
            .find_or_insert(world.spare(WORK), &mut batch, &mut inserted)
            .map(drop)
    }),
    ("find or insert by partition", |world| {
        let nrows = world.nrows();
        let mut table = world.table_mut()?;
        let (mut pending, mut refs) = (all_rows(nrows), vec![0; nrows]);
        let keys = world.keys();
        let mut batch = Batch::new(&world.hashes, &keys, &mut pending, &mut refs)?;
        let mut inserted = vec![0; nrows.div_ceil(64)];
        let mut inserted = RowMask::try_new(nrows, &mut inserted)?;
        let chunks = PARTS.map(|part| world.spare(part) as u32);
        let partitions = Partitions {
            shift: SHIFT,
            chunks: &chunks,
        };
        table
            .find_or_insert_partitioned(&partitions, &mut batch, &mut inserted)
            .map(drop)
    }),
    ("append and link", |world| {
        append_and_link(world, false, |table, chunk, from| table.link(chunk, from))
    }),
    ("append and link counting", |world| {
        append_and_link(world, false, |table, chunk, from| {
            table.link_counting(chunk, from).map(|(linked, _)| linked)
        })
    }),
    ("append and link grouped", |world| {
        append_and_link(world, false, |table, chunk, from| {
            table.link_grouped(chunk, from).map(|(linked, _)| linked)
        })
    }),
    ("append columns and link", |world| {
        append_and_link(world, true, |table, chunk, from| table.link(chunk, from))
    }),
    ("merge", |world| {
        let mut from = CHUNK_HEADER;
        let (source, chunk) = (world.spare(SOURCE), world.spare(WORK));
        // A merge that fills the index goes on in a larger one.
        for _ in 0..MOST_STEPS {
            let mut table = world.table_mut()?;
            let (_, stop) = table.merge(source, &mut from, chunk, |_, _| Ok::<_, Error>(()))?;
            if !matches!(stop, MergeStop::IndexFull) {
                return Ok(());
            }
            let capacity = 2 * world.capacity;
            let grown = Guarded::new(index_size(&world.config, capacity)?);
            regrow_into(world, grown, capacity)?;
        }
        panic!("a loop over a call without end");
    }),
    ("writer", |world| {
        let nrows = world.nrows();
        let writer = writer(&world.config, world.chunks())?;
        let keys = world.keys();
        let (mut pending, mut refs) = (all_rows(nrows), vec![0; nrows]);
        let mut batch = Batch::new(&world.hashes, &keys, &mut pending, &mut refs)?;
        let chunks = WRITER_PARTS.map(|part| world.spare(part) as u32);
        let partitions = Partitions {
            shift: SHIFT,
            chunks: &chunks,
        };
        let (mut rows, mut nulls) = ([0; 2], 0);
        world.with_columns(|columns| {
            writer.append_partitioned_columns(
                &partitions,
                columns,
                &mut batch,
                &mut Appended {
                    rows: &mut rows,
                    nulls: &mut nulls,
                },
            )
        })?;
        let (mut pending, mut refs) = (all_rows(nrows), vec![0; nrows]);
        let mut batch = Batch::new(&world.hashes, &keys, &mut pending, &mut refs)?;
        writer.append(chunks[0] as usize, Some(&world.payload), &mut batch)?;
        let (mut pending, mut refs) = (all_rows(nrows), vec![0; nrows]);
        let mut batch = Batch::new(&world.hashes, &keys, &mut pending, &mut refs)?;
        world
            .with_columns(|columns| writer.append_columns(chunks[1] as usize, columns, &mut batch))
            .map(drop)
    }),
    ("split", |world| {
        let writer = writer(&world.config, world.chunks())?;
        let chunks = SPLIT_PARTS.map(|part| world.spare(part) as u32);
        let partitions = Partitions {
            shift: SHIFT,
            chunks: &chunks,
        };
        for source in 0..world.table_chunks {
            let (mut from, mut copies, mut hashes) = (CHUNK_HEADER, [0; 16], [0; 16]);
            drain(|| {
                let split =
                    writer.split(&partitions, source, &mut from, &mut copies, &mut hashes)?;
                // A full partition's chunk ends the walk as its end does.
                Ok(if split.full.is_some() { 0 } else { split.count })
            })?;
        }
        Ok(())
    }),
    ("clear key", |world| {
        world
            .table_mut()?
            .clear_key(world.config.nkeys - 1)
            .map(drop)
    }),
    ("regrow", |world| {
        let capacity = 2 * world.capacity;
        let grown = Guarded::new(index_size(&world.config, capacity)?);
        regrow_into(world, grown, capacity)
    }),
];

/// Move the table to `grown`, an index of `capacity`, and keep it there
/// once the move is done. A regrow links every record of its chunks, so
/// the merge's source is hidden meanwhile, as a grouping hides it.
fn regrow_into(world: &mut World, grown: Guarded, capacity: u64) -> Result<(), Error> {
    let source = &world.blocks[world.spare(SOURCE)];
    let used = source.read::<u64>(0);
    source.write(0, CHUNK_HEADER as u64);
    let regrown = world.table_mut().and_then(|mut table| {
        // SAFETY: the new index is ours alone.
        unsafe { table.regrow(grown.base(), grown.len, capacity) }
    });
    world.blocks[world.spare(SOURCE)].write(0, used);
    regrown?;
    assert!(world.index.guarded(), "the old index's guard words changed");
    world.index = grown;
    world.capacity = capacity;
    Ok(())
}

/// What a damage leads the calls to.
#[derive(Clone, Copy, Debug)]
enum Expect {
    /// Only data: every call succeeds.
    Data,
    /// A call that fails gives a damaged table and one of these rules,
    /// and some call fails.
    Seen(&'static [Rule]),
    /// As `Seen`, but no call need fail: the damage may lie where no walk
    /// of this table goes.
    Maybe(&'static [Rule]),
}

/// One place damaged in one way, by its name.
type Damage = (&'static str, fn(&World), Expect);

/// Add `delta` to the `u32` at `byte` of the index.
fn bump32(world: &World, byte: usize, delta: i32) {
    let value = world.index.read::<u32>(byte);
    world.index.write(byte, value.wrapping_add_signed(delta));
}

/// Add `delta` to the `u64` at `byte` of the index.
fn bump64(world: &World, byte: usize, delta: i64) {
    let value = world.index.read::<u64>(byte);
    world.index.write(byte, value.wrapping_add_signed(delta));
}

/// The closed list of damage: the fields of the header, the count of
/// records, a bucket, the fields of a record and a used mark, each at and
/// around its limits.
const DAMAGES: &[Damage] = &[
    ("nothing", |_| {}, Expect::Data),
    // The header.
    (
        "magic",
        |w| bump64(w, MAGIC, 1),
        Expect::Seen(&[Rule::Magic]),
    ),
    (
        "version above",
        |w| bump32(w, VERSION, 1),
        Expect::Seen(&[Rule::Version]),
    ),
    (
        "version below",
        |w| bump32(w, VERSION, -1),
        Expect::Seen(&[Rule::Version]),
    ),
    (
        "header size above",
        |w| bump32(w, HEADER_SIZE, 8),
        Expect::Seen(&[Rule::HeaderSize]),
    ),
    (
        "header size below",
        |w| bump32(w, HEADER_SIZE, -8),
        Expect::Seen(&[Rule::HeaderSize]),
    ),
    (
        "index longer",
        |w| bump64(w, REGION_LEN, 8),
        Expect::Seen(&[Rule::IndexLength]),
    ),
    (
        "index shorter",
        |w| bump64(w, REGION_LEN, -8),
        Expect::Seen(&[Rule::Buckets]),
    ),
    (
        "index of odd length",
        |w| bump64(w, REGION_LEN, -4),
        Expect::Seen(&[Rule::IndexShape]),
    ),
    (
        "index of no length",
        |w| w.index.write(REGION_LEN, 0_u64),
        Expect::Seen(&[Rule::IndexShape]),
    ),
    (
        "index of most length",
        |w| w.index.write(REGION_LEN, u64::MAX),
        Expect::Seen(&[Rule::IndexLength]),
    ),
    (
        "buckets later",
        |w| bump64(w, BUCKETS_OFFSET, 8),
        Expect::Seen(&[Rule::Buckets]),
    ),
    (
        "buckets misaligned",
        |w| bump64(w, BUCKETS_OFFSET, -4),
        Expect::Seen(&[Rule::Buckets]),
    ),
    (
        "buckets at the start",
        |w| w.index.write(BUCKETS_OFFSET, 0_u64),
        Expect::Seen(&[Rule::Buckets]),
    ),
    (
        "reserved word used",
        |w| w.index.write(RESERVED_USED, 1_u64),
        Expect::Seen(&[Rule::Reserved]),
    ),
    (
        "twice the buckets",
        |w| bump32(w, NBUCKETS, w.index.read::<u32>(NBUCKETS) as i32),
        Expect::Seen(&[Rule::BucketShift]),
    ),
    (
        "half the buckets",
        |w| bump32(w, NBUCKETS, -(w.index.read::<u32>(NBUCKETS) as i32 / 2)),
        // Fewer than the least buckets, or a count the shift does not give.
        Expect::Seen(&[Rule::BucketCount, Rule::BucketShift]),
    ),
    (
        "a bucket less",
        |w| bump32(w, NBUCKETS, -1),
        Expect::Seen(&[Rule::BucketCount]),
    ),
    (
        "no bucket",
        |w| w.index.write(NBUCKETS, 0_u32),
        Expect::Seen(&[Rule::BucketCount]),
    ),
    (
        "most buckets",
        |w| w.index.write(NBUCKETS, u32::MAX),
        Expect::Seen(&[Rule::BucketCount]),
    ),
    (
        "shift above",
        |w| bump32(w, BUCKET_SHIFT, 1),
        Expect::Seen(&[Rule::BucketShift]),
    ),
    (
        "shift below",
        |w| bump32(w, BUCKET_SHIFT, -1),
        Expect::Seen(&[Rule::BucketShift]),
    ),
    (
        "shift of the hash",
        |w| w.index.write(BUCKET_SHIFT, 32_u32),
        Expect::Seen(&[Rule::BucketShift]),
    ),
    (
        "record size above",
        |w| bump32(w, RECORD_SIZE, 8),
        Expect::Seen(&[Rule::RecordSize]),
    ),
    (
        "record size below",
        |w| bump32(w, RECORD_SIZE, -8),
        Expect::Seen(&[Rule::RecordSize]),
    ),
    (
        "record size zero",
        |w| w.index.write(RECORD_SIZE, 0_u32),
        Expect::Seen(&[Rule::RecordSize]),
    ),
    (
        "record size most",
        |w| w.index.write(RECORD_SIZE, u32::MAX),
        Expect::Seen(&[Rule::RecordSize]),
    ),
    (
        "payload above",
        |w| bump32(w, PAYLOAD_SIZE, 1),
        Expect::Seen(&[Rule::RecordSize]),
    ),
    (
        "payload most",
        |w| w.index.write(PAYLOAD_SIZE, u32::MAX),
        Expect::Seen(&[Rule::RecordSize]),
    ),
    (
        "no key",
        |w| w.index.write(NKEYS, 0_u32),
        Expect::Seen(&[Rule::KeyCount]),
    ),
    (
        "a key more",
        |w| bump32(w, NKEYS, 1),
        Expect::Seen(&[Rule::RecordSize]),
    ),
    (
        "too many keys",
        |w| w.index.write(NKEYS, MAX_KEYS as u32 + 1),
        Expect::Seen(&[Rule::KeyCount]),
    ),
    (
        "a flag",
        |w| w.index.write(FLAGS, 1_u32),
        Expect::Seen(&[Rule::Flags]),
    ),
    (
        "every flag",
        |w| w.index.write(FLAGS, u32::MAX),
        Expect::Seen(&[Rule::Flags]),
    ),
    (
        "first reserved word",
        |w| w.index.write(RESERVED, 1_u64),
        Expect::Seen(&[Rule::Reserved]),
    ),
    (
        "last reserved word",
        |w| w.index.write(RESERVED + 16, u64::MAX),
        Expect::Seen(&[Rule::Reserved]),
    ),
    // The count of records.
    (
        "no record",
        |w| w.index.write(NRECORDS, 0_u64),
        Expect::Seen(&[Rule::Chain]),
    ),
    (
        "one record",
        |w| w.index.write(NRECORDS, 1_u64),
        Expect::Maybe(&[Rule::Chain]),
    ),
    (
        "a record less",
        |w| bump64(w, NRECORDS, -1),
        Expect::Maybe(&[Rule::Chain]),
    ),
    ("a record more", |w| bump64(w, NRECORDS, 1), Expect::Data),
    (
        "most records",
        |w| w.index.write(NRECORDS, u64::MAX),
        Expect::Data,
    ),
    // The bucket of the victim's hash.
    (
        "bucket emptied",
        |w| w.index.write(w.victim_bucket(), 0_u32),
        Expect::Data,
    ),
    (
        "bucket to a missing chunk",
        |w| {
            w.index
                .write(w.victim_bucket(), reference(w.lens.len(), CHUNK_HEADER))
        },
        Expect::Seen(&[Rule::Outside]),
    ),
    (
        "bucket to a used mark",
        |w| w.index.write(w.victim_bucket(), reference(1, 0)),
        Expect::Seen(&[Rule::Outside]),
    ),
    (
        "bucket past its chunk",
        |w| {
            let chunk = placement(w.victim).0;
            w.index
                .write(w.victim_bucket(), reference(chunk, w.lens[chunk] - 8))
        },
        Expect::Seen(&[Rule::Outside]),
    ),
    (
        "bucket a unit past the last record",
        |w| {
            let chunk = placement(w.victim).0;
            let byte = w.lens[chunk] - w.record_size + 8;
            w.index.write(w.victim_bucket(), reference(chunk, byte))
        },
        Expect::Seen(&[Rule::Outside]),
    ),
    (
        "bucket to the last record",
        |w| {
            let chunk = placement(w.victim).0;
            let byte = w.lens[chunk] - w.record_size;
            w.index.write(w.victim_bucket(), reference(chunk, byte))
        },
        // A record of another chain when the chunk is full, else zeros.
        Expect::Maybe(&[Rule::Misplaced]),
    ),
    (
        "bucket past the used mark",
        |w| {
            let chunk = placement(w.victim).0;
            w.index
                .write(w.victim_bucket(), reference(chunk, w.used(chunk)))
        },
        Expect::Seen(&[Rule::Misplaced, Rule::Outside]),
    ),
    (
        "bucket inside a record",
        |w| w.index.write(w.victim_bucket(), w.victim + 1),
        Expect::Seen(&[Rule::Misplaced, Rule::Outside]),
    ),
    (
        "bucket of most",
        |w| w.index.write(w.victim_bucket(), u32::MAX),
        Expect::Seen(&[Rule::Outside]),
    ),
    // The victim's record.
    (
        "next to itself",
        |w| {
            let (block, byte) = w.victim();
            block.write(byte + 4, w.victim);
        },
        Expect::Seen(&[Rule::Chain]),
    ),
    (
        "next to a missing chunk",
        |w| {
            let (block, byte) = w.victim();
            block.write(byte + 4, reference(w.lens.len(), CHUNK_HEADER));
        },
        Expect::Seen(&[Rule::Outside]),
    ),
    (
        "next past its chunk",
        |w| {
            let (chunk, byte) = placement(w.victim);
            w.blocks[chunk].write(byte + 4, reference(chunk, w.lens[chunk] - 8));
        },
        Expect::Seen(&[Rule::Outside]),
    ),
    (
        "next a unit past the last record",
        |w| {
            let (chunk, byte) = placement(w.victim);
            let last = w.lens[chunk] - w.record_size + 8;
            w.blocks[chunk].write(byte + 4, reference(chunk, last));
        },
        Expect::Seen(&[Rule::Outside]),
    ),
    (
        "next inside a record",
        |w| {
            let (block, byte) = w.victim();
            block.write(byte + 4, w.victim + 1);
        },
        Expect::Seen(&[Rule::Misplaced, Rule::Outside]),
    ),
    (
        "next of most",
        |w| {
            let (block, byte) = w.victim();
            block.write(byte + 4, u32::MAX);
        },
        Expect::Seen(&[Rule::Outside]),
    ),
    (
        "hash",
        |w| {
            let (block, byte) = w.victim();
            block.write(byte, block.read::<u32>(byte) ^ 1);
        },
        Expect::Data,
    ),
    (
        "NULL bits",
        |w| {
            let (block, byte) = w.victim();
            block.write(byte + 8, u32::MAX);
        },
        Expect::Data,
    ),
    (
        "record size field above",
        |w| {
            let (block, byte) = w.victim();
            block.write(byte + 12, block.read::<u32>(byte + 12) + 1);
        },
        Expect::Seen(&[Rule::Misplaced, Rule::NoRecord]),
    ),
    (
        "record size field zero",
        |w| {
            let (block, byte) = w.victim();
            block.write(byte + 12, 0_u32);
        },
        Expect::Seen(&[Rule::Misplaced, Rule::NoRecord]),
    ),
    (
        "record size field most",
        |w| {
            let (block, byte) = w.victim();
            block.write(byte + 12, u32::MAX);
        },
        Expect::Seen(&[Rule::Misplaced, Rule::NoRecord]),
    ),
    (
        "key",
        |w| {
            let (block, byte) = w.victim();
            block.write(byte + 16, block.read::<u64>(byte + 16) ^ 1);
        },
        Expect::Data,
    ),
    (
        "payload",
        |w| {
            let (block, byte) = w.victim();
            let at = byte + w.record_size - 8;
            block.write(at, !block.read::<u64>(at));
        },
        Expect::Data,
    ),
    // The used mark of the victim's chunk.
    (
        "used mark zero",
        |w| {
            w.victim().0.write(0, 0_u64);
        },
        Expect::Seen(&[Rule::UsedMark, Rule::BlockUsedMark]),
    ),
    (
        "used mark within itself",
        |w| {
            w.victim().0.write(0, 7_u64);
        },
        Expect::Seen(&[Rule::UsedMark, Rule::BlockUsedMark]),
    ),
    (
        "used mark off a record",
        |w| {
            let chunk = placement(w.victim).0;
            w.victim().0.write(0, w.used(chunk) as u64 - 8);
        },
        Expect::Seen(&[Rule::UsedMark, Rule::BlockUsedMark]),
    ),
    (
        "used mark past the chunk",
        |w| {
            let chunk = placement(w.victim).0;
            w.victim().0.write(0, w.lens[chunk] as u64 + 8);
        },
        Expect::Seen(&[Rule::UsedMark, Rule::BlockUsedMark]),
    ),
    (
        "used mark a record past the chunk",
        |w| {
            let chunk = placement(w.victim).0;
            w.victim()
                .0
                .write(0, (w.lens[chunk] + w.record_size) as u64);
        },
        Expect::Seen(&[Rule::UsedMark, Rule::BlockUsedMark]),
    ),
    (
        "used mark of most",
        |w| {
            w.victim().0.write(0, u64::MAX);
        },
        Expect::Seen(&[Rule::UsedMark, Rule::BlockUsedMark]),
    ),
    (
        "used mark a record less",
        |w| {
            let chunk = placement(w.victim).0;
            w.victim()
                .0
                .write(0, (w.used(chunk) - w.record_size) as u64);
        },
        Expect::Data,
    ),
    (
        "used mark at the chunk's end",
        |w| {
            let chunk = placement(w.victim).0;
            w.victim().0.write(0, w.lens[chunk] as u64);
        },
        Expect::Maybe(&[Rule::NoRecord]),
    ),
];

/// Run `run`, a call named `call`, over `world`, damaged by `damage`:
/// its outcome must be what the damage leads to, and every block's guard
/// words as they were. Note in `seen` whether it saw the damage.
fn judge(
    world: &mut World,
    (call, run): Call,
    (damage, expect): (&str, Expect),
    pass: &str,
    seen: &mut bool,
) -> Result<(), TestCaseError> {
    let result = run(world);
    prop_assert!(
        world.guarded(),
        "{damage}: {call} ({pass}) wrote past a block"
    );
    let Err(error) = result else {
        return Ok(());
    };
    let rule = (error.kind(), error.rule());
    match expect {
        Expect::Data => prop_assert!(false, "{damage}: {call} ({pass}) failed: {rule:?}"),
        Expect::Seen(rules) | Expect::Maybe(rules) => prop_assert!(
            rule.0 == Kind::Damaged && rules.contains(&rule.1),
            "{damage}: {call} ({pass}) failed with {rule:?}, not {rules:?}"
        ),
    }
    *seen = true;
    Ok(())
}

/// Damage a table of `case` in every way of the list, and run every call
/// over it: each call alone over the table just damaged, then all of
/// them in turn, twice, so that calls follow the ones that failed.
fn damage_and_call(case: &Case) -> Result<(), TestCaseError> {
    for (index, &(damage, apply, expect)) in DAMAGES.iter().enumerate() {
        // Under Miri, a few of each kind.
        if cfg!(miri) && index % 6 != 0 {
            continue;
        }
        let mut seen = false;
        for &call in CALLS {
            let mut world = World::build(case);
            apply(&world);
            judge(&mut world, call, (damage, expect), "alone", &mut seen)?;
        }
        let mut world = World::build(case);
        apply(&world);
        for round in ["first round", "second round"] {
            for &call in CALLS {
                judge(&mut world, call, (damage, expect), round, &mut seen)?;
            }
        }
        if let Expect::Seen(_) = expect {
            prop_assert!(seen, "{damage}: no call saw it");
        }
    }
    Ok(())
}

#[test]
fn every_damage_against_every_call() {
    let config = Config {
        cases: if cfg!(miri) { 1 } else { 24 },
        failure_persistence: None,
        ..Config::default()
    };
    let outcome = TestRunner::new(config).run(&cases(), |case| damage_and_call(&case));
    if let Err(failure) = outcome {
        panic!("{failure}");
    }
}
