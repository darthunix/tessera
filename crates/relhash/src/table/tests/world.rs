//! The world of the table's matrices: valid tables of four shapes in
//! blocks with guard words around them, built from a case of proptest,
//! and every call of the crate over them, each run so that it may take
//! no memory and noting its answers.

use proptest::prelude::*;

use super::*;
use crate::table::testing::{Column, Keys};

/// Words of [`PATTERN`] before and after every block.
pub(super) const GUARD: usize = 4;

/// What the words around a block hold.
pub(super) const PATTERN: u64 = 0x5a5a_a5a5_0ff0_f00f;

/// The offset of the payload's size in the header, beside the others of
/// `tests.rs`.
pub(super) const PAYLOAD_SIZE: usize = 60;

/// Bytes of payload of a record: one column, after its word of NULL bits.
pub(super) const PAYLOAD: usize = 16;

/// The shift of the partitions of the calls by partition: two, by the
/// top bit of the hash.
pub(super) const SHIFT: u32 = 31;

/// Steps of a caller's loop over a call, past which the loop is taken
/// for one without end.
pub(super) const MOST_STEPS: usize = 100_000;

/// A block between guard words, read and written only through the
/// pointer taken once from its vector.
pub(super) struct Guarded {
    pub(super) _words: Vec<u64>,
    pub(super) start: *mut u64,
    pub(super) len: usize,
}

impl Guarded {
    /// A block of `len` bytes of zeros, a multiple of 8.
    pub(super) fn new(len: usize) -> Self {
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

    pub(super) fn base(&self) -> *mut u8 {
        // SAFETY: the block starts after the guard, in the vector.
        unsafe { self.start.add(GUARD).cast() }
    }

    /// The value at `byte` of the block.
    pub(super) fn read<T: Copy>(&self, byte: usize) -> T {
        assert!(byte + size_of::<T>() <= self.len, "a read past the block");
        // SAFETY: inside the block, just checked.
        unsafe { self.base().add(byte).cast::<T>().read_unaligned() }
    }

    /// Write `value` at `byte` of the block.
    pub(super) fn write<T: Copy>(&self, byte: usize, value: T) {
        assert!(byte + size_of::<T>() <= self.len, "a write past the block");
        // SAFETY: inside the block, just checked.
        unsafe { self.base().add(byte).cast::<T>().write_unaligned(value) }
    }

    /// Whether the words around the block are as they were made.
    pub(super) fn guarded(&self) -> bool {
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
pub(super) enum Shape {
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
pub(super) struct Case {
    pub(super) shape: Shape,
    pub(super) nkeys: usize,
    pub(super) rows: Vec<(i32, i32, bool)>,
}

pub(super) fn cases() -> impl Strategy<Value = Case> {
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
pub(super) struct World {
    pub(super) config: TableConfig,
    pub(super) record_size: usize,
    pub(super) capacity: u64,
    pub(super) index: Guarded,
    pub(super) blocks: Vec<Guarded>,
    pub(super) bases: Vec<*mut u8>,
    pub(super) lens: Vec<usize>,
    /// The chunks of the table's records; the spare ones follow.
    pub(super) table_chunks: usize,
    pub(super) columns: Vec<Column>,
    pub(super) hashes: Vec<u32>,
    pub(super) payload: Vec<u8>,
    pub(super) values: Vec<u64>,
    pub(super) nulls: Vec<bool>,
    /// Every record of the table as built, and of the merge's source.
    pub(super) refs: Vec<u32>,
    /// The first record of the first chunk that holds one.
    pub(super) victim: u32,
    pub(super) filter: Guarded,
    pub(super) shared: Guarded,
    pub(super) marks: Vec<Guarded>,
    /// What the calls answered, in turn: references, counts, rows.
    pub(super) answers: Vec<u64>,
}

/// The spare chunks after the table's: the one its appends link, the
/// source of a merge, those of the calls by partition and of a chunk
/// writer's, and the targets of a split.
pub(super) const WORK: usize = 0;
pub(super) const SOURCE: usize = 1;
pub(super) const PARTS: [usize; 2] = [2, 3];
pub(super) const WRITER_PARTS: [usize; 2] = [4, 5];
pub(super) const SPLIT_PARTS: [usize; 2] = [6, 7];
/// The chunk a split shape's rows are appended to before their split.
pub(super) const BUILD: usize = 8;
pub(super) const SPARE: usize = 9;

impl World {
    pub(super) fn build(case: &Case) -> Self {
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
            answers: Vec::new(),
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
    pub(super) fn fill(&mut self, shape: Shape) {
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
    pub(super) fn scan_refs(&self) -> Vec<u32> {
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
    pub(super) fn spare(&self, spare: usize) -> usize {
        self.table_chunks + spare
    }

    pub(super) fn chunks(&self) -> Chunks<'_> {
        // SAFETY: the blocks live as long as the world, each in a vector
        // of its own, and are reached only through tables, chunk writers
        // and the damage, one at a time.
        unsafe { Chunks::new(&self.bases, &self.lens) }.unwrap()
    }

    pub(super) fn table(&self) -> Result<Table<'_>, Error> {
        // SAFETY: as `chunks`; no table writes while this one lives.
        unsafe { Table::attach(self.index.base(), self.index.len, self.chunks()) }
    }

    pub(super) fn table_mut(&self) -> Result<TableMut<'_>, Error> {
        // SAFETY: as `chunks`; this handle is the one that lives.
        unsafe { TableMut::attach_mut(self.index.base(), self.index.len, self.chunks()) }
    }

    pub(super) fn keys(&self) -> Keys<'_> {
        Keys(&self.columns)
    }

    pub(super) fn nrows(&self) -> usize {
        self.hashes.len()
    }

    /// The used mark of chunk `chunk`.
    pub(super) fn used(&self, chunk: usize) -> usize {
        self.blocks[chunk].read::<u64>(0) as usize
    }

    /// The block and byte of the victim's record.
    pub(super) fn victim(&self) -> (&Guarded, usize) {
        let (chunk, byte) = placement(self.victim);
        (&self.blocks[chunk], byte)
    }

    /// The byte of the index where the bucket of the victim's hash lies.
    pub(super) fn victim_bucket(&self) -> usize {
        let (block, byte) = self.victim();
        let hash = block.read::<u32>(byte);
        let shift = self.index.read::<u32>(BUCKET_SHIFT);
        let offset = self.index.read::<u64>(BUCKETS_OFFSET) as usize;
        offset + 4 * (hash >> shift) as usize
    }

    /// Whether the words around every block are as they were made.
    pub(super) fn guarded(&self) -> bool {
        self.index.guarded()
            && self.blocks.iter().all(Guarded::guarded)
            && self.filter.guarded()
            && self.shared.guarded()
            && self.marks.iter().all(Guarded::guarded)
    }

    /// The bytes of every block: the index, the chunks, the filters and
    /// the marks.
    pub(super) fn bytes(&self) -> Vec<u8> {
        [&self.index, &self.filter, &self.shared]
            .into_iter()
            .chain(&self.blocks)
            .chain(&self.marks)
            // SAFETY: the block's bytes, which nothing writes meanwhile.
            .flat_map(|block| unsafe { core::slice::from_raw_parts(block.base(), block.len) })
            .copied()
            .collect()
    }

    /// Call `call` with the rows' column of payload.
    pub(super) fn with_columns<T>(&self, call: impl FnOnce(&PayloadColumns<'_>) -> T) -> T {
        let (values, nulls) = ([self.values.as_slice()], [self.nulls.as_slice()]);
        call(&PayloadColumns::new(&values, &nulls, self.nrows()).unwrap())
    }
}

/// The marks of the world's chunks.
pub(super) struct MarkBlocks<'a>(pub(super) &'a [Guarded]);

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
pub(super) fn drain(mut step: impl FnMut() -> Result<usize, Error>) -> Result<(), Error> {
    for _ in 0..MOST_STEPS {
        if step()? == 0 {
            return Ok(());
        }
    }
    panic!("a loop over a call without end");
}

/// Run `calls`, calls of the crate alone, and check that they take no
/// memory, as the promise "No memory taken" says, also when they fail.
pub(super) fn quiet<T>(calls: impl FnOnce() -> T) -> T {
    let before = allocations();
    let out = calls();
    assert_eq!(allocations(), before, "a call of the crate took memory");
    out
}

/// An allocation under [`quiet`] fails the test.
#[test]
#[should_panic(expected = "took memory")]
fn an_allocation_of_a_call_is_seen() {
    quiet(|| std::hint::black_box(vec![0_u8; 8]));
}

/// Probe the rows of `rows`, then step each to its further matches with
/// `next`; the answers are the rows left and the references after each.
pub(super) fn probe(
    world: &mut World,
    rows: impl Fn(usize) -> bool,
    next: fn(&Table<'_>, &mut Matches<'_>) -> Result<usize, Error>,
) -> Result<(), Error> {
    let nrows = world.nrows();
    let mut found = words(&(0..nrows).map(rows).collect::<Vec<_>>());
    let mut refs = vec![0; nrows];
    let mut answers = Vec::new();
    let result = (|| {
        let table = quiet(|| world.table())?;
        let mut matches = Matches::new(&mut found, &mut refs)?;
        let mut left = quiet(|| table.probe(&world.hashes, &world.keys(), &mut matches))?;
        for _ in 0..MOST_STEPS {
            answers.push(left as u64);
            answers.extend(matches.refs().iter().map(|&reference| u64::from(reference)));
            if left == 0 {
                return Ok(());
            }
            left = quiet(|| next(&table, &mut matches))?;
        }
        panic!("a loop over a call without end");
    })();
    world.answers.extend(answers);
    result
}

/// Append every row to the chunk the calls link, from where its records
/// end, and link them with `link`; the answers are the references of the
/// rows and the records linked.
pub(super) fn append_and_link(
    world: &mut World,
    columns: bool,
    link: fn(&mut TableMut<'_>, usize, &mut usize) -> Result<usize, Error>,
) -> Result<(), Error> {
    let chunk = world.spare(WORK);
    let (mut pending, mut refs) = (all_rows(world.nrows()), vec![0; world.nrows()]);
    let result = (|| {
        let mut table = quiet(|| world.table_mut())?;
        // SAFETY: the chunk's block, which the table only reads meanwhile.
        let mut from = quiet(|| unsafe {
            Chunks::used(world.bases[chunk], world.lens[chunk], world.record_size)
        })?;
        let keys = world.keys();
        let mut batch = Batch::new(&world.hashes, &keys, &mut pending, &mut refs)?;
        if columns {
            world.with_columns(|columns| {
                quiet(|| table.append_columns(chunk, columns, &mut batch))
            })?;
        } else {
            quiet(|| table.append(chunk, Some(&world.payload), &mut batch))?;
        }
        quiet(|| link(&mut table, chunk, &mut from))
    })();
    world
        .answers
        .extend(refs.iter().map(|&reference| u64::from(reference)));
    world
        .answers
        .push(result.map_or(u64::MAX, |linked| linked as u64));
    result.map(drop)
}

/// A call of the crate over a world, by its name.
pub(super) type Call = (&'static str, fn(&mut World) -> Result<(), Error>);

/// Every call of the crate that reads a table's blocks. Each runs the
/// crate's calls under [`quiet`], and notes their answers in the world.
pub(super) const CALLS: &[Call] = &[
    ("attach", |world| {
        let table = quiet(|| world.table())?;
        let stats = quiet(|| table.stats());
        let checked = quiet(|| table.check_config(&world.config));
        world
            .answers
            .extend([stats.records, stats.buckets, stats.bytes_used]);
        checked
    }),
    ("attach_mut", |world| quiet(|| world.table_mut().map(drop))),
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
        let mut answers = Vec::new();
        let result = (|| {
            let table = quiet(|| world.table())?;
            let mut records = quiet(|| table.records());
            for &reference in &world.refs {
                let record = quiet(|| table.record(reference))?;
                answers.push(u64::from(record.hash));
                quiet(|| records.record(reference))?;
            }
            Ok(())
        })();
        world.answers.extend(answers);
        result
    }),
    ("scan", |world| {
        let (mut cursor, mut out, mut answers) = (Cursor::start(), [0; 7], Vec::new());
        let result = (|| {
            let table = quiet(|| world.table_mut())?;
            drain(|| {
                let count = quiet(|| table.scan(&mut cursor, &mut out))?;
                answers.extend(out[..count].iter().map(|&reference| u64::from(reference)));
                Ok(count)
            })
        })();
        world.answers.extend(answers);
        result
    }),
    ("scan unmarked", |world| {
        let (mut out, mut answers) = ([0; 7], Vec::new());
        let result = (|| {
            let table = quiet(|| world.table())?;
            let marks = MarkBlocks(&world.marks);
            let mut cursor = Cursor::start();
            drain(|| {
                let count = quiet(|| table.scan_unmarked(&mut cursor, Some(&marks), &mut out))?;
                answers.extend(out[..count].iter().map(|&reference| u64::from(reference)));
                Ok(count)
            })?;
            let (mut cursor, chunks) = (Cursor::start(), quiet(|| world.chunks()));
            drain(|| {
                quiet(|| {
                    scan_unmarked(
                        &chunks,
                        world.record_size,
                        &mut cursor,
                        Some(&marks),
                        &mut out,
                    )
                })
            })
        })();
        world.answers.extend(answers);
        result
    }),
    ("mark", |world| {
        let rows = all_rows(world.refs.len());
        let table = quiet(|| world.table())?;
        let rows = RowMaskView::try_new(world.refs.len(), &rows)?;
        let marks = MarkBlocks(&world.marks);
        quiet(|| table.mark(&world.refs, &rows, &marks))?;
        quiet(|| mark(&world.lens, world.record_size, &world.refs, &rows, &marks))
    }),
    ("used marks", |world| {
        let mut answers = Vec::new();
        let result = (|| {
            quiet(|| world.chunks().check_all())?;
            for (&base, &len) in world.bases.iter().zip(&world.lens) {
                // SAFETY: a chunk's block, which nothing writes meanwhile.
                answers
                    .push(quiet(|| unsafe { Chunks::used(base, len, world.record_size) })? as u64);
            }
            Ok(())
        })();
        world.answers.extend(answers);
        result
    }),
    ("bloom", |world| {
        let mut rows = all_rows(world.nrows());
        // SAFETY: the filter's words, reached only through this slice
        // while it lives.
        let filter = unsafe {
            core::slice::from_raw_parts_mut(world.filter.base().cast::<u64>(), world.filter.len / 8)
        };
        let result = (|| {
            quiet(|| world.table()?.bloom(filter))?;
            let mut rows = RowMask::try_new(world.nrows(), &mut rows)?;
            quiet(|| bloom::probe(filter, &world.hashes, &mut rows))
        })();
        world.answers.extend(rows);
        result.map(drop)
    }),
    ("shared bloom", |world| {
        let mut rows = all_rows(world.nrows());
        let result = (|| {
            let nwords = world.shared.len / 8;
            // SAFETY: the block's words, reached only through this filter.
            let filter = quiet(|| unsafe {
                bloom::SharedFilter::attach(world.shared.base().cast(), nwords)
            })?;
            quiet(|| filter.init());
            quiet(|| world.table()?.try_build_bloom(&filter))?;
            let mut rows = RowMask::try_new(world.nrows(), &mut rows)?;
            quiet(|| bloom::probe_shared(&filter, &world.hashes, &mut rows))
        })();
        world.answers.extend(rows);
        result.map(drop)
    }),
    ("payloads", |world| {
        let rows = all_rows(world.refs.len());
        let mut out = vec![core::ptr::null_mut(); world.refs.len()];
        let mut table = quiet(|| world.table_mut())?;
        let mut cursor = quiet(|| table.payloads());
        for &reference in &world.refs {
            quiet(|| cursor.payload(reference).map(drop))?;
            quiet(|| cursor.hold(0, reference))?;
            quiet(|| cursor.held(0).map(drop))?;
        }
        for &reference in &world.refs {
            quiet(|| table.payload_mut(reference).map(drop))?;
        }
        let rows = RowMaskView::try_new(world.refs.len(), &rows)?;
        quiet(|| table.payload_ptrs(&world.refs, &rows, &mut out))
    }),
    ("gather", |world| {
        let nrows = world.nrows();
        let (mut rows, mut refs) = (all_rows(nrows), vec![0; nrows]);
        let (mut one, mut two, mut three) = (vec![0; nrows], vec![0; nrows], vec![0; nrows]);
        let (mut values, mut nulls) = (vec![0; nrows], vec![false; nrows]);
        let result = (|| {
            let table = quiet(|| world.table())?;
            let mut matches = Matches::new(&mut rows, &mut refs)?;
            quiet(|| table.probe(&world.hashes, &world.keys(), &mut matches))?;
            quiet(|| table.gather(&matches, 8, &mut one))?;
            quiet(|| table.gather_scattered(&matches, 8, &mut two))?;
            quiet(|| table.gather_key(&matches, 0, &mut values, &mut nulls))?;
            quiet(|| table.gather_words(&matches, 0, &mut [&mut three[..]]))
        })();
        world
            .answers
            .extend(one.into_iter().chain(two).chain(three).chain(values));
        world.answers.extend(nulls.into_iter().map(u64::from));
        result
    }),
    ("each payload", |world| {
        let rows = all_rows(world.refs.len());
        let mut table = quiet(|| world.table_mut())?;
        let rows = RowMaskView::try_new(world.refs.len(), &rows)?;
        quiet(|| {
            table.each_payload(&world.refs, &rows, |row, payload| {
                payload[..8].copy_from_slice(&(row as u64).to_ne_bytes());
            })
        })
    }),
    ("append and link through a shared handle", |world| {
        let chunk = world.spare(WORK);
        let (mut pending, mut refs) = (all_rows(world.nrows()), vec![0; world.nrows()]);
        let result = (|| {
            let table = quiet(|| world.table())?;
            // SAFETY: the chunk's block, which the table only reads meanwhile.
            let mut from = quiet(|| unsafe {
                Chunks::used(world.bases[chunk], world.lens[chunk], world.record_size)
            })?;
            let keys = world.keys();
            let mut batch = Batch::new(&world.hashes, &keys, &mut pending, &mut refs)?;
            // SAFETY: this handle is the chunk's one writer and linker, and
            // no record of it is borrowed.
            quiet(|| unsafe { table.append(chunk, Some(&world.payload), &mut batch) })?;
            world.with_columns(|columns| {
                // SAFETY: as above.
                quiet(|| unsafe { table.append_columns(chunk, columns, &mut batch) })
            })?;
            // SAFETY: as above.
            quiet(|| unsafe { table.link_counting(chunk, &mut from) })
        })();
        world
            .answers
            .extend(refs.iter().map(|&reference| u64::from(reference)));
        result.map(drop)
    }),
    ("find or insert", |world| find_or_insert(world, false)),
    ("find or insert by partition", |world| {
        find_or_insert(world, true)
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
            let merged = quiet(|| {
                world
                    .table_mut()?
                    .merge(source, &mut from, chunk, |_, _| Ok::<_, Error>(()))
            });
            let (count, stop) = merged?;
            world.answers.extend([count as u64, from as u64]);
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
        let chunks = WRITER_PARTS.map(|part| world.spare(part) as u32);
        let (mut counts, mut nulls) = ([0; 2], 0);
        let mut batches: Vec<(Vec<u64>, Vec<u32>)> =
            (0..3).map(|_| (all_rows(nrows), vec![0; nrows])).collect();
        let result = (|| {
            let writer = quiet(|| writer(&world.config, world.chunks()))?;
            let keys = world.keys();
            let partitions = Partitions {
                shift: SHIFT,
                chunks: &chunks,
            };
            let [first, second, third] = batches.as_mut_slice() else {
                unreachable!()
            };
            let mut batch = Batch::new(&world.hashes, &keys, &mut first.0, &mut first.1)?;
            world.with_columns(|columns| {
                quiet(|| {
                    writer.append_partitioned_columns(
                        &partitions,
                        columns,
                        &mut batch,
                        &mut Appended {
                            rows: &mut counts,
                            nulls: &mut nulls,
                        },
                    )
                })
            })?;
            let mut batch = Batch::new(&world.hashes, &keys, &mut second.0, &mut second.1)?;
            quiet(|| writer.append(chunks[0] as usize, Some(&world.payload), &mut batch))?;
            let mut batch = Batch::new(&world.hashes, &keys, &mut third.0, &mut third.1)?;
            world.with_columns(|columns| {
                quiet(|| writer.append_columns(chunks[1] as usize, columns, &mut batch))
            })
        })();
        world.answers.extend(counts);
        for (_, refs) in &batches {
            world
                .answers
                .extend(refs.iter().map(|&reference| u64::from(reference)));
        }
        result.map(drop)
    }),
    ("split", |world| {
        let chunks = SPLIT_PARTS.map(|part| world.spare(part) as u32);
        let mut answers = Vec::new();
        let result = (|| {
            let writer = quiet(|| writer(&world.config, world.chunks()))?;
            let partitions = Partitions {
                shift: SHIFT,
                chunks: &chunks,
            };
            for source in 0..world.table_chunks {
                let (mut from, mut copies, mut hashes) = (CHUNK_HEADER, [0; 16], [0; 16]);
                drain(|| {
                    let split = quiet(|| {
                        writer.split(&partitions, source, &mut from, &mut copies, &mut hashes)
                    })?;
                    answers.extend(copies[..split.count].iter().map(|&copy| u64::from(copy)));
                    // A full partition's chunk ends the walk as its end does.
                    Ok(if split.full.is_some() { 0 } else { split.count })
                })?;
            }
            Ok(())
        })();
        world.answers.extend(answers);
        result
    }),
    ("clear key", |world| {
        let cleared = quiet(|| world.table_mut()?.clear_key(world.config.nkeys - 1))?;
        world.answers.push(cleared);
        Ok(())
    }),
    ("regrow", |world| {
        let capacity = 2 * world.capacity;
        let grown = Guarded::new(index_size(&world.config, capacity)?);
        regrow_into(world, grown, capacity)
    }),
];

/// Find or insert every row, into the chunk the calls link or by
/// partition; the answers are the references of the rows and the rows
/// inserted.
pub(super) fn find_or_insert(world: &mut World, partitioned: bool) -> Result<(), Error> {
    let nrows = world.nrows();
    let (mut pending, mut refs) = (all_rows(nrows), vec![0; nrows]);
    let mut inserted = vec![0; nrows.div_ceil(64)];
    let chunks = PARTS.map(|part| world.spare(part) as u32);
    let result = (|| {
        let mut table = quiet(|| world.table_mut())?;
        let keys = world.keys();
        let mut batch = Batch::new(&world.hashes, &keys, &mut pending, &mut refs)?;
        let mut inserted = RowMask::try_new(nrows, &mut inserted)?;
        if partitioned {
            let partitions = Partitions {
                shift: SHIFT,
                chunks: &chunks,
            };
            quiet(|| table.find_or_insert_partitioned(&partitions, &mut batch, &mut inserted))
        } else {
            quiet(|| table.find_or_insert(world.spare(WORK), &mut batch, &mut inserted))
        }
    })();
    world
        .answers
        .extend(refs.iter().map(|&reference| u64::from(reference)));
    world.answers.extend(inserted);
    result.map(drop)
}

/// Move the table to `grown`, an index of `capacity`, and keep it there
/// once the move is done. A regrow links every record of its chunks, so
/// the merge's source is hidden meanwhile, as a grouping hides it.
pub(super) fn regrow_into(world: &mut World, grown: Guarded, capacity: u64) -> Result<(), Error> {
    let source = &world.blocks[world.spare(SOURCE)];
    let used = source.read::<u64>(0);
    source.write(0, CHUNK_HEADER as u64);
    let regrown = quiet(|| {
        world.table_mut().and_then(|mut table| {
            // SAFETY: the new index is ours alone.
            unsafe { table.regrow(grown.base(), grown.len, capacity) }
        })
    });
    world.blocks[world.spare(SOURCE)].write(0, used);
    regrown?;
    assert!(world.index.guarded(), "the old index's guard words changed");
    world.index = grown;
    world.capacity = capacity;
    Ok(())
}
