//! The damage matrix: the file of a set of four partitions that the
//! crate wrote, damaged in one place from a closed list, every field of
//! its trailer, its lists, its blocks' headers and its packed bodies at
//! and around its limits, and every read of the crate run over it twice.
//! A read ends with its data or with damaged bytes of the rule the damage
//! breaks; it asks for no byte past the file or past the regions it was
//! given; a damage that is more than data is seen by some read; and every
//! rule of damaged bytes the reads give is some damage's.
#![cfg(test)]

use std::collections::HashSet;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use relhash::RECORD_SIZE_OFFSET;

use super::{
    Config, ENTRY_SIZE, Entry, FINGERPRINT, Host, Owned, ReadConfig, ReadStep, Reader,
    TRAILER_SIZE, columns_chunk, config, fill, read_back, records,
};
use crate::pack::{BYTE, CONSTANT, HALF, NEXT_LANE, WORD, ZERO};
use crate::spill_columns::ValueEnds;
use crate::spill_columns::tests::{Batch, Cell, read_batch, written};
use crate::testing::quiet;
use crate::{
    BlockHeader, BlockKind, Bounds, Error, FINGERPRINT_AT, HEADER_SIZE, KIND_AT, Kind, LEN_AT,
    LEVEL_AT, MAGIC_AT, MAX_LEVEL, NUMBER_AT, PACKED_AT, PARTITION_AT, Rule, VERSION_AT, columns,
    spill_columns,
};

/// A read's own outcome.
type Read<T> = core::result::Result<T, Error>;

/// A block read back: its header and its body.
type ReadBlock = (BlockHeader, Vec<u8>);

/// Each partition's blocks: their kinds, numbers and bodies.
type Blocks = Vec<Vec<(BlockKind, u32, Vec<u8>)>>;

/// The partitions of the set: values, chunks of columns, records, none.
const PARTITIONS: u32 = 4;
const VALUES: u32 = 0;
const COLUMNS: u32 = 1;
const RECORDS: u32 = 2;
const EMPTY: u32 = 3;

/// Steps a call may take before the matrix takes it for one that does
/// not end; the file takes a few dozen.
const STEPS: usize = 1000;

/// The time the whole matrix may take, a hundred times what it takes:
/// past it, a call inside the crate is taken for one that does not end.
const DEADLINE: Duration = Duration::from_secs(60);

/// The bound of a chunk the set's writer gives: past the most rows a
/// chunk of columns holds, so that a body of more rows reaches its check.
const CHUNK_BOUND: u64 = 16 << 20;

/// A host whose values by reference begin with the count of the bytes
/// after it, as a varlena of PostgreSQL begins with its length: a value
/// ends inside its chunk when the chunk holds that many more.
struct Lengths;

impl ValueEnds for Lengths {
    fn ends(&self, _: usize, rest: &[u8]) -> bool {
        rest.first().is_some_and(|&len| rest.len() > len as usize)
    }
}

/// Ten rows of three columns, the second by reference: a NULL in every
/// third row of the first, every fourth of the second; a value of the
/// second a length and that many bytes, of 1 to 7.
fn pair_batch() -> (Batch, Vec<bool>) {
    let columns = (0..3)
        .map(|column| {
            (0..10_u8)
                .map(|row| match column {
                    0 if row % 3 == 0 => Cell::Null,
                    1 if row % 4 == 1 => Cell::Null,
                    1 => {
                        let len = row % 7 + 1;
                        let mut value = vec![len];
                        value.extend((0..len).map(|byte| row ^ byte));
                        Cell::Bytes(value)
                    }
                    _ => Cell::Word(u64::from(row) * 1000 + column),
                })
                .collect()
        })
        .collect();
    let batch = Batch {
        columns,
        by_value: false,
    };
    (batch, vec![true, false, true])
}

/// A chunk of columns as its host wrote it: its number, the words and
/// rows it expects back, and which of its columns are by value.
struct Chunk {
    number: u32,
    words: usize,
    rows: usize,
    by_value: Vec<bool>,
}

/// The file the matrix damages, as the crate wrote it, and where its
/// parts lie.
struct Layout {
    file: Vec<u8>,
    /// The bounds the writer gives its readers.
    bounds: Bounds,
    /// Each partition's blocks in their order, and their places.
    blocks: Vec<Vec<(Owned, Entry)>>,
    /// The chunks of columns of partition `COLUMNS`, in their order.
    chunks: Vec<Chunk>,
    /// Where the lists start.
    lists: usize,
}

impl Layout {
    /// A set of four partitions, its blocks of each kind written in turn:
    /// values of four lengths; a chunk of columns with its values, one
    /// without, one whose every lane packs to a constant, and one of
    /// lanes of 2 and 4 bytes; two chunks of records that pack, lanes of
    /// bytes and of words and halves, and one stored as it is; and a
    /// partition of no block. A page of two entries reads its lists in
    /// several steps.
    fn new() -> Result<Self> {
        let mut host = Host::new(Config {
            bounds: Bounds::all(CHUNK_BOUND),
            ..config(PARTITIONS, 512, false)
        })?;
        let (batch, by_value) = pair_batch();
        let (pair, values, words) = written(&batch, 10)?;
        let (alone_batch, alone_by_value) = read_batch(6, 1);
        let (alone, no_values, alone_words) = written(&alone_batch, 6)?;
        ensure!(no_values.is_empty(), "a chunk by value with values");
        let blocks = [
            (BlockKind::Values, 1, VALUES, vec![1_u8; 16]),
            (BlockKind::Records, 9, RECORDS, records(8, 40, 7)),
            (BlockKind::Values, 5, COLUMNS, values),
            (BlockKind::Values, 2, VALUES, vec![2; 40]),
            (BlockKind::Columns, 5, COLUMNS, pair),
            (
                BlockKind::Records,
                10,
                RECORDS,
                (0..64).map(|byte| byte as u8).collect(),
            ),
            (BlockKind::Columns, 6, COLUMNS, alone),
            (BlockKind::Values, 3, VALUES, vec![3; 8]),
            (BlockKind::Columns, 7, COLUMNS, columns_chunk(4, 1, 0)),
            (BlockKind::Values, 4, VALUES, vec![4; 512]),
            (BlockKind::Records, 11, RECORDS, records(8, 40, 0x1_2345)),
            (
                BlockKind::Columns,
                8,
                COLUMNS,
                columns_chunk(5, 3, 0x1_0000),
            ),
        ];
        let mut placed = vec![Vec::new(); PARTITIONS as usize];
        for (kind, number, partition, body) in blocks {
            let block = Owned {
                kind,
                number,
                partition,
                body,
            };
            let place = host.write(&block)?.context("a write cut")?;
            placed[partition as usize].push((block, place));
        }
        ensure!(host.finish()?);
        Ok(Self {
            bounds: host.writer.read_bounds(),
            lists: host.writer.bytes() as usize,
            file: host.file,
            blocks: placed,
            chunks: vec![
                Chunk {
                    number: 5,
                    words,
                    rows: 10,
                    by_value,
                },
                Chunk {
                    number: 6,
                    words: alone_words,
                    rows: 6,
                    by_value: alone_by_value,
                },
                Chunk {
                    number: 7,
                    words: 1,
                    rows: 4,
                    by_value: vec![true],
                },
                Chunk {
                    number: 8,
                    words: 3,
                    rows: 5,
                    by_value: vec![true; 3],
                },
            ],
        })
    }

    /// Where block `index` of `partition` starts.
    fn block(&self, partition: u32, index: usize) -> usize {
        self.blocks[partition as usize][index].1.offset as usize
    }

    /// Where the stored body of block `index` of `partition` starts.
    fn body(&self, partition: u32, index: usize) -> usize {
        self.block(partition, index) + HEADER_SIZE
    }

    fn trailer(&self) -> usize {
        self.file.len() - TRAILER_SIZE
    }

    /// Where the count of `partition`'s blocks lies.
    fn count(&self, partition: u32) -> usize {
        self.lists + 8 * partition as usize
    }

    /// Where entry `index` of `partition`'s list lies.
    fn entry(&self, partition: u32, index: usize) -> usize {
        let before: usize = self.blocks[..partition as usize].iter().map(Vec::len).sum();
        self.lists + 8 * PARTITIONS as usize + ENTRY_SIZE * (before + index)
    }

    fn config(&self, partition: u32) -> ReadConfig {
        ReadConfig {
            partitions: PARTITIONS,
            level: 2,
            partition,
            fingerprint: FINGERPRINT,
            bounds: self.bounds,
        }
    }
}

fn get32(file: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes(file[at..at + 4].try_into().expect("4 bytes"))
}

fn get64(file: &[u8], at: usize) -> u64 {
    u64::from_ne_bytes(file[at..at + 8].try_into().expect("8 bytes"))
}

fn put32(file: &mut [u8], at: usize, value: u32) {
    file[at..at + 4].copy_from_slice(&value.to_ne_bytes());
}

fn put64(file: &mut [u8], at: usize, value: u64) {
    file[at..at + 8].copy_from_slice(&value.to_ne_bytes());
}

fn bump32(file: &mut [u8], at: usize, delta: i32) {
    put32(file, at, get32(file, at).wrapping_add_signed(delta));
}

fn bump64(file: &mut [u8], at: usize, delta: i64) {
    put64(file, at, get64(file, at).wrapping_add_signed(delta));
}

/// The lane of the records' length in a packed body of records.
const LENGTH_LANE: usize = RECORD_SIZE_OFFSET / 4;

/// Where each lane's data starts in the packed body of records at `at`,
/// by its codes: [`crate::pack`] draws the body.
fn record_lanes(file: &[u8], at: usize) -> Vec<usize> {
    let (count, len) = (get32(file, at) as usize, get32(file, at + 4) as usize);
    let lanes = len / 4;
    let mut data = at + 8 + lanes.next_multiple_of(4);
    let mut starts = Vec::new();
    for lane in 0..lanes {
        starts.push(data);
        data += match file[at + 8 + lane] {
            ZERO => 0,
            CONSTANT => 4,
            BYTE => count,
            HALF => 2 * count,
            _ => 4 * count,
        };
    }
    starts
}

/// The first lane of the packed body of records at `at` stored by `code`.
fn lane_of(file: &[u8], at: usize, code: u8) -> usize {
    let len = get32(file, at + 4) as usize;
    (0..len / 4)
        .find(|&lane| lane != LENGTH_LANE && file[at + 8 + lane] == code)
        .expect("a lane of the code")
}

/// Where the descriptor of lane `lane` of a packed chunk of columns at
/// `at` lies: its width, then its least value 8 bytes on.
fn descriptor(at: usize, lane: usize) -> usize {
    at + 8 + 16 * lane
}

/// Where each lane's values start in the packed chunk of columns at `at`,
/// by its widths: [`columns::pack`] draws the body.
fn column_lanes(file: &[u8], at: usize) -> Vec<usize> {
    let (rows, words) = (get32(file, at) as usize, get32(file, at + 4) as usize);
    let lanes = columns::lanes(words);
    let mut data = at + 8 + 16 * lanes;
    let mut starts = Vec::new();
    for lane in 0..lanes {
        starts.push(data);
        data += (file[descriptor(at, lane)] as usize * rows).next_multiple_of(8);
    }
    starts
}

/// The most bytes a reader takes a block for: the larger bound, a
/// header and the slack of packing.
fn most_block(bounds: Bounds) -> u64 {
    bounds.largest() + (HEADER_SIZE + columns::pack_slack(columns::MAX_WORDS)) as u64
}

/// The lane of the words of column 1 of the chunk of columns with its
/// values, references to them, which NULL rows' words of 0 make 8 bytes
/// wide.
const REFERENCES: usize = 2;

/// What a damage leads the reads to.
#[derive(Clone, Copy, Debug)]
enum Expect {
    /// Only data: every read succeeds.
    Data,
    /// Some read fails, each that fails with one of these rules.
    Seen(&'static [Rule]),
}

/// One place damaged in one way, by its name.
type Damage = (&'static str, fn(&mut Vec<u8>, &Layout), Expect);

/// The closed list of damage, each field at and around its limits: the
/// trailer, the counts and the entries of the lists, the fields of a
/// block's header, and the counts, codes, descriptors and lanes of the
/// packed bodies.
const DAMAGES: &[Damage] = &[
    ("nothing", |_, _| {}, Expect::Data),
    // The trailer.
    (
        "a file shorter than a trailer",
        |f, _| f.truncate(TRAILER_SIZE - 8),
        Expect::Seen(&[Rule::Trailer]),
    ),
    (
        "a file a word shorter",
        |f, _| f.truncate(f.len() - 8),
        Expect::Seen(&[Rule::TrailerMagic]),
    ),
    (
        "a file a word longer",
        |f, _| f.extend_from_slice(&[0; 8]),
        Expect::Seen(&[Rule::TrailerMagic]),
    ),
    (
        "trailer magic",
        |f, l| bump64(f, l.trailer(), 1),
        Expect::Seen(&[Rule::TrailerMagic]),
    ),
    (
        "lists past the file",
        |f, l| put64(f, l.trailer() + 8, l.file.len() as u64),
        Expect::Seen(&[Rule::TrailerLists]),
    ),
    (
        "lists of most",
        |f, l| put64(f, l.trailer() + 8, u64::MAX),
        Expect::Seen(&[Rule::TrailerLists]),
    ),
    (
        "lists a word later",
        |f, l| bump64(f, l.trailer() + 8, 8),
        Expect::Seen(&[Rule::ListsRoom]),
    ),
    (
        "lists a word earlier",
        |f, l| bump64(f, l.trailer() + 8, -8),
        Expect::Seen(&[Rule::ListsRoom]),
    ),
    (
        "lists an entry later",
        |f, l| bump64(f, l.trailer() + 8, ENTRY_SIZE as i64),
        Expect::Seen(&[Rule::ListCounts]),
    ),
    (
        "lists an entry earlier",
        |f, l| bump64(f, l.trailer() + 8, -(ENTRY_SIZE as i64)),
        Expect::Seen(&[Rule::ListCounts]),
    ),
    (
        "lists of no room for their counts",
        |f, l| put64(f, l.trailer() + 8, l.trailer() as u64 - 16),
        Expect::Seen(&[Rule::ListsRoom]),
    ),
    (
        "partitions one more",
        |f, l| bump64(f, l.trailer() + 16, 1),
        Expect::Seen(&[Rule::TrailerPartitions]),
    ),
    (
        "partitions one fewer",
        |f, l| bump64(f, l.trailer() + 16, -1),
        Expect::Seen(&[Rule::TrailerPartitions]),
    ),
    (
        "file fingerprint",
        |f, l| bump64(f, l.trailer() + 24, 1),
        Expect::Seen(&[Rule::FileFingerprint]),
    ),
    // The counts of the lists.
    (
        "a count one more",
        |f, l| bump64(f, l.count(VALUES), 1),
        Expect::Seen(&[Rule::ListCounts]),
    ),
    (
        "a count one fewer",
        |f, l| bump64(f, l.count(VALUES), -1),
        Expect::Seen(&[Rule::ListCounts]),
    ),
    (
        "a count of most",
        |f, l| put64(f, l.count(RECORDS), u64::MAX),
        Expect::Seen(&[Rule::ListCounts]),
    ),
    (
        "a block in the partition of none",
        |f, l| bump64(f, l.count(EMPTY), 1),
        Expect::Seen(&[Rule::ListCounts]),
    ),
    (
        "a block counted in the next partition",
        |f, l| {
            bump64(f, l.count(VALUES), -1);
            bump64(f, l.count(COLUMNS), 1);
        },
        Expect::Seen(&[Rule::EntryPlace, Rule::SeekPosition]),
    ),
    // The entries of the lists.
    (
        "an entry a word later",
        |f, l| bump64(f, l.entry(VALUES, 0), 8),
        Expect::Seen(&[Rule::Magic, Rule::SeekPosition]),
    ),
    (
        "an entry a word earlier",
        |f, l| bump64(f, l.entry(VALUES, 1), -8),
        Expect::Seen(&[Rule::Magic, Rule::SeekPosition]),
    ),
    (
        "an entry into the lists",
        |f, l| put64(f, l.entry(VALUES, 3), l.lists as u64 - 8),
        Expect::Seen(&[Rule::EntryPlace]),
    ),
    (
        "an entry of most offset",
        |f, l| put64(f, l.entry(RECORDS, 1), u64::MAX),
        Expect::Seen(&[Rule::EntryPlace]),
    ),
    (
        "an entry a word longer",
        |f, l| bump64(f, l.entry(VALUES, 0) + 8, 8),
        Expect::Seen(&[Rule::BlockStored]),
    ),
    (
        "an entry a word shorter",
        |f, l| bump64(f, l.entry(VALUES, 0) + 8, -8),
        Expect::Seen(&[Rule::BlockStored]),
    ),
    (
        "an entry shorter than a header",
        |f, l| put64(f, l.entry(VALUES, 0) + 8, HEADER_SIZE as u64 - 8),
        Expect::Seen(&[Rule::EntryLength]),
    ),
    (
        "an entry of no length",
        |f, l| put64(f, l.entry(VALUES, 0) + 8, 0),
        Expect::Seen(&[Rule::EntryLength]),
    ),
    (
        "an entry of most length",
        |f, l| put64(f, l.entry(COLUMNS, 1) + 8, u64::MAX),
        Expect::Seen(&[Rule::EntryLength]),
    ),
    (
        "an entry a word past the largest",
        |f, l| put64(f, l.entry(COLUMNS, 1) + 8, most_block(l.bounds) + 8),
        Expect::Seen(&[Rule::EntryLength]),
    ),
    (
        "two entries swapped",
        |f, l| {
            let (first, second) = (l.entry(VALUES, 0), l.entry(VALUES, 1));
            let entry: Vec<u8> = f[first..first + ENTRY_SIZE].to_vec();
            f.copy_within(second..second + ENTRY_SIZE, first);
            f[second..second + ENTRY_SIZE].copy_from_slice(&entry);
        },
        Expect::Seen(&[Rule::EntryPlace]),
    ),
    // The header of a block.
    (
        "magic",
        |f, l| bump64(f, l.block(VALUES, 0) + MAGIC_AT, 1),
        Expect::Seen(&[Rule::Magic]),
    ),
    (
        "version above",
        |f, l| bump32(f, l.block(VALUES, 0) + VERSION_AT, 1),
        Expect::Seen(&[Rule::Version]),
    ),
    (
        "version below",
        |f, l| bump32(f, l.block(VALUES, 0) + VERSION_AT, -1),
        Expect::Seen(&[Rule::Version]),
    ),
    (
        "a kind past the kinds",
        |f, l| {
            put32(
                f,
                l.block(VALUES, 0) + KIND_AT,
                BlockKind::Columns as u32 + 1,
            )
        },
        Expect::Seen(&[Rule::BlockKind]),
    ),
    (
        "a kind of none",
        |f, l| put32(f, l.block(VALUES, 0) + KIND_AT, 0),
        Expect::Seen(&[Rule::BlockKind]),
    ),
    (
        "values read as records",
        |f, l| put32(f, l.block(VALUES, 0) + KIND_AT, BlockKind::Records as u32),
        Expect::Data,
    ),
    (
        "a pair's values read as records",
        |f, l| put32(f, l.block(COLUMNS, 0) + KIND_AT, BlockKind::Records as u32),
        Expect::Seen(&[Rule::PairKind]),
    ),
    (
        "a chunk of columns read as values",
        |f, l| put32(f, l.block(COLUMNS, 1) + KIND_AT, BlockKind::Values as u32),
        Expect::Seen(&[Rule::PackedRecords]),
    ),
    (
        "a pair's rows read as values, the lengths too",
        |f, l| {
            let at = l.block(COLUMNS, 1);
            let packed = get32(f, at + PACKED_AT);
            put32(f, at + KIND_AT, BlockKind::Values as u32);
            put32(f, at + PACKED_AT, 0);
            put64(f, at + LEN_AT, u64::from(packed));
        },
        Expect::Seen(&[Rule::PairKind]),
    ),
    (
        "packed records read as columns",
        |f, l| put32(f, l.block(RECORDS, 0) + KIND_AT, BlockKind::Columns as u32),
        Expect::Seen(&[Rule::PackedHeader]),
    ),
    (
        "values numbered on",
        |f, l| bump32(f, l.block(VALUES, 0) + NUMBER_AT, 1),
        Expect::Data,
    ),
    (
        "a pair's values numbered on",
        |f, l| bump32(f, l.block(COLUMNS, 0) + NUMBER_AT, 1),
        Expect::Seen(&[Rule::PairNumber]),
    ),
    (
        "a pair's rows numbered on",
        |f, l| bump32(f, l.block(COLUMNS, 1) + NUMBER_AT, 1),
        Expect::Seen(&[Rule::PairNumber]),
    ),
    (
        "partition",
        |f, l| bump32(f, l.block(VALUES, 0) + PARTITION_AT, 1),
        Expect::Seen(&[Rule::BlockPartition]),
    ),
    (
        "level",
        |f, l| bump32(f, l.block(VALUES, 0) + LEVEL_AT, 1),
        Expect::Seen(&[Rule::BlockLevel]),
    ),
    (
        "level past the bits of a hash",
        |f, l| put32(f, l.block(VALUES, 0) + LEVEL_AT, MAX_LEVEL),
        Expect::Seen(&[Rule::Level]),
    ),
    (
        "fingerprint",
        |f, l| bump64(f, l.block(VALUES, 0) + FINGERPRINT_AT, 1),
        Expect::Seen(&[Rule::Fingerprint]),
    ),
    (
        "length a word longer",
        |f, l| bump64(f, l.block(VALUES, 0) + LEN_AT, 8),
        Expect::Seen(&[Rule::BlockStored]),
    ),
    (
        "length a word shorter",
        |f, l| bump64(f, l.block(VALUES, 0) + LEN_AT, -8),
        Expect::Seen(&[Rule::BlockStored]),
    ),
    (
        "length of no 8s",
        |f, l| bump64(f, l.block(VALUES, 0) + LEN_AT, -4),
        Expect::Seen(&[Rule::BlockLength]),
    ),
    (
        "length past the bound of values",
        |f, l| put64(f, l.block(VALUES, 1) + LEN_AT, l.bounds.values + 8),
        Expect::Seen(&[Rule::BlockLength]),
    ),
    (
        "length of most",
        |f, l| put64(f, l.block(VALUES, 1) + LEN_AT, u64::MAX),
        Expect::Seen(&[Rule::BlockLength]),
    ),
    (
        "a chunk past the bound of chunks",
        |f, l| put64(f, l.block(RECORDS, 0) + LEN_AT, l.bounds.chunk + 8),
        Expect::Seen(&[Rule::BlockLength]),
    ),
    (
        "a chunk of columns shorter than its header",
        |f, l| put64(f, l.block(COLUMNS, 1) + LEN_AT, columns::HEADER as u64 - 8),
        Expect::Seen(&[Rule::ColumnsBlock]),
    ),
    (
        "values said packed",
        |f, l| put32(f, l.block(VALUES, 0) + PACKED_AT, 16),
        Expect::Seen(&[Rule::PackedRecords]),
    ),
    (
        "records shorter than a used mark",
        |f, l| put64(f, l.block(RECORDS, 1) + LEN_AT, 0),
        Expect::Seen(&[Rule::UsedMark]),
    ),
    (
        "records stored as they are, said packed",
        |f, l| put32(f, l.block(RECORDS, 1) + PACKED_AT, 16),
        Expect::Seen(&[Rule::BlockStored]),
    ),
    (
        "records packed into their length",
        |f, l| {
            let len = get64(f, l.block(RECORDS, 0) + LEN_AT);
            put32(f, l.block(RECORDS, 0) + PACKED_AT, len as u32);
        },
        Expect::Seen(&[Rule::PackedRecords]),
    ),
    (
        "records packed into a word",
        |f, l| put32(f, l.block(RECORDS, 0) + PACKED_AT, 8),
        Expect::Seen(&[Rule::PackedRecords]),
    ),
    (
        "records packed a word longer",
        |f, l| bump32(f, l.block(RECORDS, 0) + PACKED_AT, 8),
        Expect::Seen(&[Rule::BlockStored]),
    ),
    (
        "records packed into no 8s",
        |f, l| bump32(f, l.block(RECORDS, 0) + PACKED_AT, -4),
        Expect::Seen(&[Rule::PackedRecords]),
    ),
    (
        "columns packed into nothing",
        |f, l| put32(f, l.block(COLUMNS, 1) + PACKED_AT, 0),
        Expect::Seen(&[Rule::ColumnsBlock]),
    ),
    (
        "columns packed into half a word",
        |f, l| put32(f, l.block(COLUMNS, 1) + PACKED_AT, 4),
        Expect::Seen(&[Rule::ColumnsBlock]),
    ),
    (
        "columns packed into no 8s",
        |f, l| bump32(f, l.block(COLUMNS, 1) + PACKED_AT, -4),
        Expect::Seen(&[Rule::ColumnsBlock]),
    ),
    // The packed body of records: its counts, its codes and its lanes.
    (
        "records one more",
        |f, l| bump32(f, l.body(RECORDS, 0), 1),
        Expect::Seen(&[Rule::PackedHeader]),
    ),
    (
        "records a word longer",
        |f, l| bump32(f, l.body(RECORDS, 0) + 4, 8),
        Expect::Seen(&[Rule::PackedHeader]),
    ),
    (
        "records of 5 bytes",
        |f, l| {
            put32(f, l.body(RECORDS, 0), 64);
            put32(f, l.body(RECORDS, 0) + 4, 5);
        },
        Expect::Seen(&[Rule::PackedFill]),
    ),
    (
        "records of 16 bytes",
        |f, l| {
            put32(f, l.body(RECORDS, 0), 20);
            put32(f, l.body(RECORDS, 0) + 4, 16);
        },
        Expect::Seen(&[Rule::PackedFill]),
    ),
    (
        "records of 28 bytes, the header too",
        |f, l| {
            put32(f, l.body(RECORDS, 0) + 4, 28);
            put64(f, l.block(RECORDS, 0) + LEN_AT, 8 + 8 * 28);
        },
        Expect::Seen(&[Rule::PackedFill]),
    ),
    (
        "records too long for their codes",
        |f, l| {
            put32(f, l.body(RECORDS, 0), 1);
            put32(f, l.body(RECORDS, 0) + 4, 8000);
            put64(f, l.block(RECORDS, 0) + LEN_AT, 8 + 8000);
        },
        Expect::Seen(&[Rule::LaneCodes]),
    ),
    (
        "one record more, the header too",
        |f, l| {
            bump32(f, l.body(RECORDS, 0), 1);
            bump64(f, l.block(RECORDS, 0) + LEN_AT, 40);
        },
        Expect::Seen(&[Rule::RecordLength, Rule::LaneEnd]),
    ),
    (
        "a code past the codes",
        |f, l| {
            let lane = lane_of(f, l.body(RECORDS, 0), BYTE);
            f[l.body(RECORDS, 0) + 8 + lane] = WORD + 1;
        },
        Expect::Seen(&[Rule::LaneCode]),
    ),
    (
        "the lane of the next records stored",
        |f, l| f[l.body(RECORDS, 0) + 8 + NEXT_LANE] = CONSTANT,
        Expect::Seen(&[Rule::NextLane]),
    ),
    (
        "the padding of the codes",
        |f, l| {
            let lanes = record_lanes(f, l.body(RECORDS, 0)).len();
            f[l.body(RECORDS, 0) + 8 + lanes] = 1;
        },
        Expect::Data,
    ),
    (
        "the last lane wider",
        |f, l| {
            let lanes = record_lanes(f, l.body(RECORDS, 0)).len();
            f[l.body(RECORDS, 0) + 8 + lanes - 1] = WORD;
        },
        Expect::Seen(&[Rule::LaneEnd]),
    ),
    (
        "the last lane of bytes empty",
        |f, l| {
            let at = l.body(RECORDS, 0);
            let lanes = record_lanes(f, at).len();
            let last = (0..lanes).rev().find(|&lane| f[at + 8 + lane] == BYTE);
            f[at + 8 + last.expect("a lane of bytes")] = ZERO;
        },
        Expect::Seen(&[Rule::PackedTail]),
    ),
    (
        "the lane of the length a lane of bytes",
        |f, l| f[l.body(RECORDS, 0) + 8 + LENGTH_LANE] = BYTE,
        Expect::Seen(&[Rule::RecordLength]),
    ),
    (
        "records of another length in their lane",
        |f, l| {
            let at = record_lanes(f, l.body(RECORDS, 0))[LENGTH_LANE];
            bump32(f, at, 1);
        },
        Expect::Seen(&[Rule::RecordLength]),
    ),
    (
        "a lane of words read as halves",
        |f, l| {
            let lane = lane_of(f, l.body(RECORDS, 2), WORD);
            f[l.body(RECORDS, 2) + 8 + lane] = HALF;
        },
        Expect::Seen(&[Rule::RecordLength]),
    ),
    (
        "the last lane of halves read as words",
        |f, l| {
            let at = l.body(RECORDS, 2);
            let lanes = record_lanes(f, at).len();
            let last = (0..lanes).rev().find(|&lane| f[at + 8 + lane] == HALF);
            f[at + 8 + last.expect("a lane of halves")] = WORD;
        },
        Expect::Seen(&[Rule::LaneEnd]),
    ),
    (
        "a value of a lane of records",
        |f, l| {
            let lane = lane_of(f, l.body(RECORDS, 0), BYTE);
            let at = record_lanes(f, l.body(RECORDS, 0))[lane];
            f[at] ^= 1;
        },
        Expect::Data,
    ),
    (
        "a byte of records stored as they are",
        |f, l| f[l.body(RECORDS, 1) + 8] ^= 1,
        Expect::Data,
    ),
    // The packed body of a chunk of columns: its counts, its
    // descriptors and its lanes.
    (
        "rows one more",
        |f, l| bump32(f, l.body(COLUMNS, 1), 1),
        Expect::Seen(&[Rule::PackedHeader]),
    ),
    (
        "words one more",
        |f, l| bump32(f, l.body(COLUMNS, 1) + 4, 1),
        Expect::Seen(&[Rule::PackedHeader]),
    ),
    (
        "columns a word longer",
        |f, l| bump64(f, l.block(COLUMNS, 1) + LEN_AT, 8),
        Expect::Seen(&[Rule::PackedHeader]),
    ),
    (
        "more words than a chunk holds",
        |f, l| {
            put32(f, l.body(COLUMNS, 1), 1);
            put32(f, l.body(COLUMNS, 1) + 4, columns::MAX_WORDS as u32 + 1);
            let len = columns::size(1, columns::MAX_WORDS + 1);
            put64(f, l.block(COLUMNS, 1) + LEN_AT, len as u64);
        },
        Expect::Seen(&[Rule::ColumnsLimits]),
    ),
    (
        "more rows than a chunk holds",
        |f, l| {
            put32(f, l.body(COLUMNS, 3), columns::MAX_ROWS as u32 + 1);
            let len = columns::size(columns::MAX_ROWS + 1, 1);
            put64(f, l.block(COLUMNS, 3) + LEN_AT, len as u64);
        },
        Expect::Seen(&[Rule::ColumnsLimits]),
    ),
    (
        "descriptors past the body",
        |f, l| {
            put32(f, l.body(COLUMNS, 1), 1);
            put32(f, l.body(COLUMNS, 1) + 4, 20);
            put64(f, l.block(COLUMNS, 1) + LEN_AT, columns::size(1, 20) as u64);
        },
        Expect::Seen(&[Rule::Descriptors]),
    ),
    (
        "a width of none",
        |f, l| f[descriptor(l.body(COLUMNS, 1), REFERENCES)] = 3,
        Expect::Seen(&[Rule::LaneWidth]),
    ),
    (
        "a lane wider",
        |f, l| f[descriptor(l.body(COLUMNS, 1), 1)] = 8,
        Expect::Seen(&[Rule::ColumnsLaneEnd]),
    ),
    (
        "a lane of NULL bits empty",
        |f, l| f[descriptor(l.body(COLUMNS, 1), 0)] = 0,
        Expect::Seen(&[Rule::ColumnsTail]),
    ),
    (
        "a lane of 4 bytes read as one of 2",
        |f, l| f[descriptor(l.body(COLUMNS, 4), 1)] = 2,
        Expect::Seen(&[Rule::ColumnsTail]),
    ),
    (
        "the zeros of a descriptor",
        |f, l| f[descriptor(l.body(COLUMNS, 1), 1) + 1] = 1,
        Expect::Data,
    ),
    // The words of a chunk of columns read back.
    (
        "references a byte later",
        |f, l| bump64(f, descriptor(l.body(COLUMNS, 1), REFERENCES) + 8, 1),
        Expect::Seen(&[Rule::ValueRef]),
    ),
    (
        "references past the values",
        |f, l| bump64(f, descriptor(l.body(COLUMNS, 1), REFERENCES) + 8, 8 << 10),
        Expect::Seen(&[Rule::ValueRef]),
    ),
    (
        "references to another chunk",
        |f, l| bump64(f, descriptor(l.body(COLUMNS, 1), REFERENCES) + 8, 1 << 32),
        Expect::Seen(&[Rule::ValueRef]),
    ),
    (
        "a reference's difference",
        |f, l| {
            let at = column_lanes(f, l.body(COLUMNS, 1))[REFERENCES];
            f[at] ^= 1;
        },
        Expect::Seen(&[Rule::ValueRef]),
    ),
    (
        "a value longer than its values",
        |f, l| f[l.body(COLUMNS, 0)] = u8::MAX,
        Expect::Seen(&[Rule::ValueEnd]),
    ),
    (
        "a constant chunk a row shorter, the header too",
        |f, l| {
            bump32(f, l.body(COLUMNS, 3), -1);
            put64(f, l.block(COLUMNS, 3) + LEN_AT, columns::size(3, 1) as u64);
        },
        Expect::Seen(&[Rule::ReadRows]),
    ),
    (
        "a constant chunk of no words, its rows and a lane to match",
        |f, l| {
            let at = l.body(COLUMNS, 3);
            put32(f, at, 8);
            put32(f, at + 4, 0);
            f[descriptor(at, 0)] = 2;
        },
        Expect::Seen(&[Rule::ReadWords]),
    ),
    (
        "a least word of a column by value",
        |f, l| bump64(f, descriptor(l.body(COLUMNS, 1), 1) + 8, 1),
        Expect::Data,
    ),
    (
        "a constant of NULL bits",
        |f, l| put64(f, descriptor(l.body(COLUMNS, 3), 0) + 8, 1),
        Expect::Data,
    ),
    // The bytes of values.
    (
        "a byte of values",
        |f, l| f[l.body(VALUES, 0)] ^= 1,
        Expect::Data,
    ),
    (
        "a byte of a pair's values",
        |f, l| f[l.body(COLUMNS, 0) + 1] ^= 1,
        Expect::Data,
    ),
];

/// The rules of damaged bytes no damage of a file the crate wrote
/// reaches through its reader, and why: an earlier check of the same
/// bytes refuses first.
const UNREACHED: &[(Rule, &str)] = &[
    (
        Rule::PackedCounts,
        "a header takes no packed body of records shorter than 16 bytes",
    ),
    (
        Rule::ColumnsCounts,
        "a header takes no packed chunk of columns shorter than 8 bytes",
    ),
    (
        Rule::ColumnsFill,
        "the reader checks the same counts against the header first",
    ),
    (
        Rule::ColumnsChunk,
        "a chunk read back has the header unpacking writes, by its counts",
    ),
    (
        Rule::ColumnsMagic,
        "a chunk read back has the header unpacking writes, by its counts",
    ),
    (
        Rule::ColumnsShape,
        "a chunk read back has the header unpacking writes, by its counts",
    ),
];

/// A host that reads a partition back, and checks that every read a step
/// asks for lies in the file and in the region it names.
struct Back<'a> {
    reader: Reader,
    file: &'a [u8],
    page: Vec<Entry>,
    block: Vec<u8>,
    bounds: Bounds,
    /// The length of the body the last header gave, until it is taken.
    body: Option<usize>,
}

impl<'a> Back<'a> {
    fn new(file: &'a [u8], layout: &Layout, partition: u32) -> Result<Self> {
        Ok(Self {
            reader: Reader::file(layout.config(partition), file.len() as u64)?,
            file,
            page: vec![Entry::default(); 2],
            block: Vec::new(),
            bounds: layout.bounds,
            body: None,
        })
    }

    /// Read what a step asks into its region: false for a step that asks
    /// for nothing.
    fn serve(&mut self, step: ReadStep) -> Result<bool> {
        let (at, len, region) = match step {
            ReadStep::Page { at, len } => (at, len, self.page.len() * ENTRY_SIZE),
            ReadStep::Block { at, len } => (at, len, self.block.len()),
            _ => return Ok(false),
        };
        let end = at
            .checked_add(len as u64)
            .context("a read past every byte")?;
        ensure!(
            end <= self.file.len() as u64,
            "{step:?} past the file of {} bytes",
            self.file.len()
        );
        ensure!(len <= region, "{step:?} past its region of {region} bytes");
        let bytes = &self.file[at as usize..end as usize];
        match step {
            ReadStep::Page { .. } => fill(&mut self.page, bytes),
            _ => self.block[..len].copy_from_slice(bytes),
        }
        Ok(true)
    }

    /// Make a call again, serving its steps, until it gives what it found.
    fn run(
        &mut self,
        call: impl Fn(&mut Reader, &[Entry], &[u8]) -> Read<ReadStep>,
    ) -> Result<Read<ReadStep>> {
        for _ in 0..STEPS {
            let step = match quiet(|| call(&mut self.reader, &self.page, &self.block)) {
                Ok(step) => step,
                Err(error) => return Ok(Err(error)),
            };
            if let ReadStep::Ready { largest } = step {
                let most = most_block(self.bounds);
                ensure!(largest <= most, "a block of {largest} bytes, past {most}");
                self.block = vec![0; largest as usize];
                return Ok(Ok(step));
            }
            if !self.serve(step)? {
                return Ok(Ok(step));
            }
        }
        bail!("a call that does not end")
    }

    fn open(&mut self) -> Result<Read<ReadStep>> {
        self.run(|reader, page, _| reader.open(page))
    }

    /// The next block's header and body, or `None` at the end.
    fn next(&mut self) -> Result<Read<Option<ReadBlock>>> {
        let header = match self.run(|reader, page, block| reader.next(page, block))? {
            Err(error) => return Ok(Err(error)),
            Ok(ReadStep::End) => return Ok(Ok(None)),
            Ok(ReadStep::Header(header)) => header,
            Ok(step) => bail!("{step:?} for a block"),
        };
        self.body = Some(usize::try_from(header.len)?);
        Ok(self.body()?.map(|body| Some((header, body))))
    }

    /// The body of the block whose header the last step gave.
    fn body(&mut self) -> Result<Read<Vec<u8>>> {
        let mut body = vec![0; self.body.unwrap_or(0)];
        let taken = quiet(|| self.reader.body(&self.block, &mut body));
        if taken.is_ok() {
            self.body = None;
        }
        Ok(taken.map(|()| body))
    }
}

/// What one pass of the reads saw: the rule of the first error of each
/// read; each partition's blocks as read to the end, and as each seek to
/// a block read it, the last first; and every window of every chunk of
/// columns.
#[derive(Debug, Default, PartialEq)]
struct Seen {
    rules: Vec<Rule>,
    blocks: Vec<Vec<ReadBlock>>,
    seeks: Vec<Vec<Option<ReadBlock>>>,
    windows: Vec<(Vec<u64>, Vec<bool>)>,
}

impl Seen {
    /// Take the first error of a read: damaged bytes, or a seek to a
    /// place the lists no longer name. Any other is a broken promise.
    fn refused(&mut self, error: Error) -> Result<()> {
        ensure!(
            error.kind() == Kind::Damaged || error.rule() == Rule::SeekPosition,
            "a wrong call over a damaged file: {error}"
        );
        self.rules.push(error.rule());
        Ok(())
    }
}

/// Every read of the crate over `file`: each partition opened and read to
/// its end, its chunks of columns read as a node reads them, then a seek
/// to each of its blocks where the writer placed it, the last first, each
/// of which must give the block the read to the end gave there. After a
/// read that failed the body is asked for again and three blocks more,
/// which must end and keep to the file and the regions, whatever they
/// give.
fn pass(file: &[u8], layout: &Layout) -> Result<Seen> {
    let mut seen = Seen::default();
    for partition in 0..PARTITIONS {
        let mut back = Back::new(file, layout, partition)?;
        let mut blocks = Vec::new();
        let mut failed = false;
        match back.open()? {
            Err(error) => {
                seen.refused(error)?;
                failed = true;
            }
            Ok(ReadStep::Empty) => {}
            Ok(ReadStep::Ready { .. }) => loop {
                match back.next()? {
                    Ok(Some(block)) => blocks.push(block),
                    Ok(None) => break,
                    Err(error) => {
                        seen.refused(error)?;
                        failed = true;
                        break;
                    }
                }
                ensure!(blocks.len() < STEPS, "a partition that does not end");
            },
            Ok(step) => bail!("{step:?} for an open"),
        }
        if failed {
            let _ = back.body()?;
            for _ in 0..3 {
                let _ = back.next()?;
            }
        } else if partition == COLUMNS
            && let Err(error) = chunks(&blocks, layout, &mut seen.windows)?
        {
            seen.refused(error)?;
        }
        let seeks = seek(file, layout, partition, &mut seen)?;
        for (index, found) in seeks.iter().rev().enumerate() {
            if let (Some(found), Some(read)) = (found, blocks.get(index)) {
                ensure!(found == read, "a seek to block {index} found another");
            }
        }
        seen.blocks.push(blocks);
        seen.seeks.push(seeks);
    }
    Ok(seen)
}

/// The chunks of columns of a partition read back as a node reads them:
/// each pair of blocks by kind and number, its chunk checked once with
/// its values, then every word of every row by windows of 64 rows, each
/// of which must be the chunk's lanes. The reads of the partition end at
/// the first that fails, as a node drops the partition then; a partition
/// of fewer blocks than were written ends early: what its lists say it
/// holds is data.
fn chunks(
    blocks: &[ReadBlock],
    layout: &Layout,
    windows: &mut Vec<(Vec<u64>, Vec<bool>)>,
) -> Result<Read<()>> {
    let mut read = blocks.iter();
    for chunk in &layout.chunks {
        let Some((first, body)) = read.next() else {
            return Ok(Ok(()));
        };
        if let Err(error) = spill_columns::check_pair(first, 0, chunk.number) {
            return Ok(Err(error));
        }
        let (values, rows) = if first.kind == BlockKind::Values {
            let Some((second, rows)) = read.next() else {
                return Ok(Ok(()));
            };
            if let Err(error) = spill_columns::check_pair(second, 1, chunk.number) {
                return Ok(Err(error));
            }
            (&body[..], rows)
        } else {
            (&[][..], body)
        };
        let shape = match quiet(|| {
            spill_columns::check(
                rows,
                chunk.words,
                Some(chunk.rows),
                &chunk.by_value,
                values,
                &Lengths,
            )
        }) {
            Ok(shape) => shape,
            Err(error) => return Ok(Err(error)),
        };
        // A chunk the check passed is read unchecked: a window fails only
        // by its caller's bounds, and gives the words as the lanes hold
        // them, a NULL's as 0.
        for word in 0..shape.words {
            for first in (0..shape.rows).step_by(64) {
                let count = 64.min(shape.rows - first);
                let (mut out, mut nulls) = (vec![7; count], vec![true; count]);
                quiet(|| spill_columns::window(rows, word, first, &mut out, &mut nulls))?;
                for (at, (&got, &null)) in out.iter().zip(&nulls).enumerate() {
                    let row = first + at;
                    let bits = get64(rows, shape.lane_at(word / 64) + 8 * row);
                    let lane = get64(rows, shape.lane_at(shape.word_lane(word)) + 8 * row);
                    let is_null = bits >> (word % 64) & 1 != 0;
                    ensure!(
                        (got, null) == (if is_null { 0 } else { lane }, is_null),
                        "word {word} of row {row}: {got}, {null}"
                    );
                }
                windows.push((out, nulls));
            }
        }
    }
    Ok(Ok(()))
}

/// A seek to each block of `partition` where the writer placed it, the
/// last first, and the block it read, if any. The first error is taken;
/// the calls after it go on.
fn seek(
    file: &[u8],
    layout: &Layout,
    partition: u32,
    seen: &mut Seen,
) -> Result<Vec<Option<ReadBlock>>> {
    let places = &layout.blocks[partition as usize];
    let mut back = Back::new(file, layout, partition)?;
    match back.open()? {
        Ok(ReadStep::Ready { .. }) => {}
        // The read of the partition saw the open's outcome already.
        _ => return Ok(Vec::new()),
    }
    let mut found = Vec::new();
    let mut failed = false;
    for (_, place) in places.iter().rev() {
        let offset = place.offset;
        let block = match back.run(|reader, page, _| reader.seek(page, offset))? {
            Ok(_) => back.next()?,
            Err(error) => Err(error),
        };
        match block {
            Ok(block) => found.push(block),
            Err(error) => {
                found.push(None);
                if !failed {
                    seen.refused(error)?;
                    failed = true;
                }
            }
        }
    }
    Ok(found)
}

/// The blocks of every partition as the writer wrote them, as they read
/// back.
fn written_blocks(layout: &Layout) -> Result<Blocks> {
    layout
        .blocks
        .iter()
        .map(|blocks| {
            blocks
                .iter()
                .map(|(block, _)| Ok((block.kind, block.number, read_back(block)?)))
                .collect()
        })
        .collect()
}

/// The kind, the number and the body of each block.
fn as_written(blocks: &[Vec<ReadBlock>]) -> Blocks {
    blocks
        .iter()
        .map(|blocks| {
            blocks
                .iter()
                .map(|(header, body)| (header.kind, header.number, body.clone()))
                .collect()
        })
        .collect()
}

/// Each damage of the closed list, against every read of the crate, twice:
/// the file as written reads back whole, by its order and by seeks; a
/// damage of data is read without an error; one past data is seen by
/// some read, each read that fails failing by a rule of the damage's;
/// both passes see the same; and every rule of damaged bytes the reads
/// of the crate give is seen, but those no damage of a written file
/// reaches, each with its reason. The matrix runs in a thread of its own
/// with a deadline, so that a call that does not end fails it, and one
/// that panics, naming the damage.
#[test]
fn every_damage_against_every_read() -> Result<()> {
    let at = Arc::new(Mutex::new("the file as written"));
    let (done, outcome) = mpsc::channel();
    let name = Arc::clone(&at);
    std::thread::spawn(move || {
        let _ = done.send(matrix(&name));
    });
    let outcome = outcome.recv_timeout(DEADLINE);
    let name = at.lock().map_or("?", |name| *name);
    match outcome {
        Ok(outcome) => outcome,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            bail!("{name}: the reads did not end in {DEADLINE:?}")
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => bail!("{name}: a read panicked"),
    }
}

/// The matrix, the damage under way in `at`.
fn matrix(at: &Mutex<&'static str>) -> Result<()> {
    let layout = Layout::new()?;
    let clean = pass(&layout.file, &layout)?;
    ensure!(
        clean.rules.is_empty(),
        "the file as written: {:?}",
        clean.rules
    );
    ensure!(
        as_written(&clean.blocks) == written_blocks(&layout)?,
        "the file read back"
    );
    let reversed: Vec<Vec<ReadBlock>> = clean
        .blocks
        .iter()
        .map(|blocks| blocks.iter().rev().cloned().collect())
        .collect();
    let sought: Vec<Vec<ReadBlock>> = clean
        .seeks
        .iter()
        .map(|seeks| seeks.iter().flatten().cloned().collect())
        .collect();
    ensure!(sought == reversed, "the file read back by seeks");
    let mut all = HashSet::new();
    let mut names = HashSet::new();
    let mut wrong = Vec::new();
    for &(name, damage, expect) in DAMAGES {
        ensure!(names.insert(name), "{name}: twice in the list");
        if let Ok(mut at) = at.lock() {
            *at = name;
        }
        let mut file = layout.file.clone();
        damage(&mut file, &layout);
        ensure!(
            name == "nothing" || file != layout.file,
            "{name}: the damage changed nothing"
        );
        let first = pass(&file, &layout).with_context(|| name)?;
        let second = pass(&file, &layout).with_context(|| name)?;
        ensure!(first == second, "{name}: the passes differ");
        let held = match expect {
            Expect::Data => first.rules.is_empty(),
            Expect::Seen(rules) => {
                !first.rules.is_empty() && first.rules.iter().all(|rule| rules.contains(rule))
            }
        };
        if !held {
            wrong.push(format!("{name}: {:?} for {expect:?}", first.rules));
        }
        all.extend(first.rules);
    }
    ensure!(wrong.is_empty(), "{}", wrong.join("\n"));
    for (rule, _) in UNREACHED {
        ensure!(!all.contains(rule), "{rule:?} is reached");
    }
    let unreached: HashSet<String> = UNREACHED
        .iter()
        .map(|(rule, _)| format!("{rule:?}"))
        .collect();
    let seen: HashSet<String> = all.iter().map(|rule| format!("{rule:?}")).collect();
    for rule in rules_of_reads()? {
        ensure!(
            seen.contains(&rule) || unreached.contains(&rule),
            "no damage gives {rule}"
        );
    }
    Ok(())
}

/// The sources of the reads of the crate: the header of a block, the
/// reader of a set, the unpacking of records and of columns, and the
/// checks of a chunk of columns read back.
const SOURCES: [(&str, &str); 5] = [
    ("lib.rs", include_str!("../../lib.rs")),
    ("set/reader.rs", include_str!("../reader.rs")),
    ("pack.rs", include_str!("../../pack.rs")),
    ("columns.rs", include_str!("../../columns.rs")),
    ("spill_columns.rs", include_str!("../../spill_columns.rs")),
];

/// The errors of the sources whose rule is no literal one, and the rules
/// each gives: a header's checks give their rules to `refuse`, and a
/// chunk read back refuses by the rules of its shape.
const SPREAD: &[(&str, &[&str])] = &[
    ("Error::new(kind,rule)", &[]),
    (
        "damaged(error.rule())",
        &["ColumnsChunk", "ColumnsMagic", "ColumnsShape"],
    ),
];

/// The rules of damaged bytes that the reads of the crate give, as their
/// sources name them, read with every space taken out: a literal rule
/// given to `damaged`, to `refuse`, or with the kind of damaged bytes to
/// `Error::new`, and the rules of the errors of [`SPREAD`]. An error of
/// damaged bytes or of a kind not written out, of any other form, fails:
/// its rules would go unseen.
fn rules_of_reads() -> Result<HashSet<String>> {
    let mut rules = HashSet::new();
    for (file, source) in SOURCES {
        let code = source.split("mod tests {").next().unwrap_or(source);
        let code: String = code
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .flat_map(str::chars)
            .filter(|c| !c.is_whitespace())
            .collect();
        for mark in ["damaged(", "refuse(", "Error::new("] {
            for (at, _) in code.match_indices(mark) {
                let rest = &code[at + mark.len()..];
                let literal = match mark {
                    "Error::new(" => rest.strip_prefix("Kind::Damaged,Rule::"),
                    _ => rest.strip_prefix("Rule::"),
                };
                if let Some(rest) = literal {
                    let end = rest
                        .find(|c: char| !c.is_ascii_alphanumeric())
                        .unwrap_or(rest.len());
                    rules.insert(rest[..end].to_owned());
                } else if mark != "Error::new(" || !rest.starts_with("Kind::WrongCall,") {
                    let spread = SPREAD
                        .iter()
                        .find(|(form, _)| code[at..].starts_with(form))
                        .with_context(|| {
                            format!(
                                "{file}: an error the scan cannot read: {}",
                                &code[at..][..40]
                            )
                        })?;
                    rules.extend(spread.1.iter().map(|rule| (*rule).to_owned()));
                }
            }
        }
    }
    Ok(rules)
}

/// The rules the sources name are the ones the reads give: a rule of a
/// header, of the lists, of each packed body and of a chunk read back,
/// whatever spaces the sources put in a call; no wrong call among them.
#[test]
fn the_rules_of_reads_are_read_from_their_sources() -> Result<()> {
    let rules = rules_of_reads()?;
    for rule in [
        "Magic",
        "BlockLength",
        "Level",
        "TrailerMagic",
        "EntryPlace",
        "LaneCode",
        "LaneWidth",
        "ValueRef",
        "ColumnsShape",
    ] {
        ensure!(rules.contains(rule), "{rule} not in {rules:?}");
    }
    ensure!(!rules.contains("ReaderState"), "a wrong call among them");
    Ok(())
}
