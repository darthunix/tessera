//! The measured kernels of a spill: a chunk of records packed and
//! unpacked, a chunk of columns packed and unpacked, a batch's rows
//! appended by partition to chunks of columns, and a block's header
//! written and read; each case against a plain loop of the same bytes.

use std::hint::black_box;

use anyhow::{Context, Result, ensure};
use relspill::{BlockHeader, BlockKind, HEADER_SIZE, columns};
use tessera_core::RowMask;
use tessera_kernels::spill_columns::append_partitioned;
use tessera_kernels::table::{Partitions, PayloadColumns};

use crate::support::runner::Runner;

/// xorshift64*, from a fixed seed: the same fixtures in every run.
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

/// A chunk of `count` records of `len` bytes as the hash table lays them
/// out: its used mark, then for each record a hash, a next link, NULL
/// bits and the length in words, and then the lanes of 4 bytes `word`
/// gives by record and lane.
fn records(count: usize, len: usize, mut word: impl FnMut(usize, usize) -> u32) -> Vec<u8> {
    let mut bytes = vec![0_u8; 8 + count * len];
    let total = bytes.len() as u64;
    bytes[..8].copy_from_slice(&total.to_ne_bytes());
    for record in 0..count {
        let base = 8 + record * len;
        for lane in 0..len / 4 {
            let value = match lane {
                1 => 12345,
                3 => (len / 8) as u32,
                _ => word(record, lane),
            };
            bytes[base + lane * 4..base + lane * 4 + 4].copy_from_slice(&value.to_ne_bytes());
        }
    }
    bytes
}

/// The columns of a batch: per column a word per row and NULL flags,
/// about one row in ten NULL, words of up to `bits` bits.
struct Batch {
    values: Vec<Vec<u64>>,
    nulls: Vec<Vec<bool>>,
    hashes: Vec<u32>,
}

impl Batch {
    fn new(words: usize, nrows: usize, bits: u32, random: &mut Random) -> Self {
        let mut values = Vec::with_capacity(words);
        let mut nulls = Vec::with_capacity(words);
        for _ in 0..words {
            values.push((0..nrows).map(|_| random.next() >> (64 - bits)).collect());
            nulls.push(
                (0..nrows)
                    .map(|_| random.next().is_multiple_of(10))
                    .collect(),
            );
        }
        let hashes = (0..nrows).map(|_| (random.next() >> 32) as u32).collect();
        Self {
            values,
            nulls,
            hashes,
        }
    }

    fn nrows(&self) -> usize {
        self.hashes.len()
    }
}

/// Append every row of `batch` to `chunks` by partition, at `shift` of
/// the hash: the count appended.
fn append(chunks: &mut [Vec<u8>], batch: &Batch, shift: u32, numbers: &[u32]) -> Result<usize> {
    let values: Vec<&[u64]> = batch.values.iter().map(Vec::as_slice).collect();
    let nulls: Vec<&[bool]> = batch.nulls.iter().map(Vec::as_slice).collect();
    let columns = PayloadColumns::new(&values, &nulls, batch.nrows())?;
    let mut words = vec![u64::MAX; batch.nrows().div_ceil(64)];
    if !batch.nrows().is_multiple_of(64) {
        words[batch.nrows() / 64] = (1 << (batch.nrows() % 64)) - 1;
    }
    let mut pending = RowMask::try_new(batch.nrows(), &mut words)?;
    let mut offsets = vec![0_u32; batch.nrows()];
    let mut rows = vec![0_u64; numbers.len()];
    let partitions = Partitions {
        shift,
        chunks: numbers,
    };
    append_partitioned(
        chunks,
        &partitions,
        &batch.hashes,
        &columns,
        &mut pending,
        &mut offsets,
        &mut rows,
    )
}

/// The word of lane `lane` of row `place` of a chunk of columns.
fn lane_word(chunk: &[u8], lane: usize, place: usize) -> Result<u64> {
    let shape = columns::shape(chunk)?;
    let at = shape.lane_at(lane) + 8 * place;
    Ok(u64::from_ne_bytes(chunk[at..at + 8].try_into()?))
}

pub fn bench(runner: &mut Runner) -> Result<()> {
    let mut random = Random(0x9E37_79B9_7F4A_7C15);
    measure_records(runner, &mut random)?;
    measure_columns(runner, &mut random)?;
    measure_partitioned(runner, &mut random)?;
    measure_header(runner)
}

/// A chunk of records packed and unpacked: a join's record of an int4
/// key and an int4 value, whose lanes pack to a few bytes, and a wider
/// record of words of 20 bits.
fn measure_records(runner: &mut Runner, random: &mut Random) -> Result<()> {
    let cases = [
        (
            "join-int4",
            records(1000, 40, |record, lane| match lane {
                0 => (record as u32).wrapping_mul(2_654_435_761),
                4 => 1_000_000 + record as u32,
                8 => 7 * record as u32 + 100_000,
                _ => 0,
            }),
        ),
        (
            "words-20-bits",
            records(1000, 56, |_, lane| {
                if lane % 2 == 0 {
                    (random.next() >> 44) as u32
                } else {
                    0
                }
            }),
        ),
    ];
    for (name, chunk) in cases {
        let mut packed = vec![0_u8; chunk.len()];
        let len = relspill::pack(&chunk, &mut packed).context("the chunk packs")?;
        let mut back = vec![0_u8; chunk.len()];
        relspill::unpack(&packed[..len], &mut back)?;
        let mut expected = chunk.clone();
        let record = (chunk.len() - 8) / 1000;
        for at in (8..chunk.len()).step_by(record) {
            expected[at + 4..at + 8].fill(0);
        }
        ensure!(back == expected, "{name}: unpacked differs");
        let mut out = vec![0_u8; chunk.len()];
        let mut group = runner.group(format!("spill/records/{name}"));
        group.op("pack", || relspill::pack(black_box(&chunk), &mut out))?;
        group.op("unpack", || {
            relspill::unpack(black_box(&packed[..len]), &mut back).is_ok()
        })?;
        group.op("reference", || {
            out.copy_from_slice(black_box(&chunk));
            out[8]
        })?;
    }
    Ok(())
}

/// A chunk of columns packed and unpacked: 4 words of 1000 rows, and 70
/// words of 500 rows, whose NULL bits take two lanes.
fn measure_columns(runner: &mut Runner, random: &mut Random) -> Result<()> {
    for (words, nrows) in [(4, 1000), (70, 500)] {
        let batch = Batch::new(words, nrows, 24, random);
        let mut chunks = vec![vec![0_u8; columns::size(nrows, words)]];
        columns::init(&mut chunks[0], words)?;
        ensure!(
            append(&mut chunks, &batch, 0, &[0])? == nrows,
            "a row left out"
        );
        let chunk = &chunks[0];
        let mut packed = vec![0_u8; columns::pack_bound(nrows, words)];
        let (len, unpacked) = columns::pack(chunk, &mut packed)?;
        let mut back = vec![0_u8; unpacked];
        columns::unpack(&packed[..len], &mut back)?;
        for lane in 0..columns::lanes(words) {
            for place in 0..nrows {
                ensure!(
                    lane_word(chunk, lane, place)? == lane_word(&back, lane, place)?,
                    "{words} words: lane {lane} of row {place} differs"
                );
            }
        }
        let mut out = vec![0_u8; chunk.len()];
        let mut group = runner.group(format!("spill/columns/{words}-words"));
        group.op("pack", || {
            columns::pack(black_box(chunk), &mut packed).is_ok()
        })?;
        group.op("unpack", || {
            columns::unpack(black_box(&packed[..len]), &mut back).is_ok()
        })?;
        group.op("reference", || {
            out.copy_from_slice(black_box(chunk));
            out[0]
        })?;
    }
    Ok(())
}

/// The lanes of the rows of each partition as a plain loop keeps them:
/// per partition, per lane, a word per place.
struct Plain {
    lanes: Vec<Vec<u64>>,
    rows: Vec<usize>,
    capacity: usize,
}

impl Plain {
    /// Every row of `batch` to its partition's lanes: NULL bits first,
    /// then the words, 0 for a NULL.
    #[inline(never)]
    fn append(&mut self, batch: &Batch, shift: u32) -> usize {
        let partitions = self.rows.len();
        let words = batch.values.len();
        let null_lanes = columns::null_lanes(words);
        self.rows.fill(0);
        for row in 0..batch.nrows() {
            let partition = (batch.hashes[row] >> shift) as usize & (partitions - 1);
            let place = self.rows[partition];
            let lanes = &mut self.lanes[partition];
            for lane in 0..null_lanes {
                lanes[lane * self.capacity + place] = 0;
            }
            for word in 0..words {
                let null = batch.nulls[word][row];
                lanes[(word / 64) * self.capacity + place] |= u64::from(null) << (word % 64);
                lanes[(null_lanes + word) * self.capacity + place] =
                    if null { 0 } else { batch.values[word][row] };
            }
            self.rows[partition] += 1;
        }
        batch.nrows()
    }
}

/// A batch of 1024 rows appended to 8 partitions' chunks of columns, of
/// 4 words and of 70; every call starts from empty chunks.
fn measure_partitioned(runner: &mut Runner, random: &mut Random) -> Result<()> {
    const PARTITIONS: usize = 8;
    const ROWS: usize = 1024;
    let numbers: Vec<u32> = (0..PARTITIONS as u32).collect();
    for words in [4, 70] {
        let batch = Batch::new(words, ROWS, 32, random);
        let mut chunks = vec![vec![0_u8; columns::size(ROWS, words)]; PARTITIONS];
        for chunk in &mut chunks {
            columns::init(chunk, words)?;
        }
        ensure!(
            append(&mut chunks, &batch, 3, &numbers)? == ROWS,
            "a row left out"
        );
        let mut plain = Plain {
            lanes: vec![vec![0; columns::lanes(words) * ROWS]; PARTITIONS],
            rows: vec![0; PARTITIONS],
            capacity: ROWS,
        };
        plain.append(&batch, 3);
        for (partition, chunk) in chunks.iter().enumerate() {
            let rows = columns::shape(chunk)?.rows;
            ensure!(rows == plain.rows[partition], "partition {partition}: rows");
            for lane in 0..columns::lanes(words) {
                for place in 0..rows {
                    ensure!(
                        lane_word(chunk, lane, place)?
                            == plain.lanes[partition][lane * ROWS + place],
                        "partition {partition}: lane {lane} of row {place} differs"
                    );
                }
            }
        }
        let values: Vec<&[u64]> = batch.values.iter().map(Vec::as_slice).collect();
        let nulls: Vec<&[bool]> = batch.nulls.iter().map(Vec::as_slice).collect();
        let columns = PayloadColumns::new(&values, &nulls, ROWS)?;
        let mut mask = vec![0_u64; ROWS / 64];
        let mut offsets = vec![0_u32; ROWS];
        let mut rows = vec![0_u64; PARTITIONS];
        let partitions = Partitions {
            shift: 3,
            chunks: &numbers,
        };
        let mut group = runner.group(format!("spill/partitioned/{words}-words"));
        group.op("append", || {
            for chunk in chunks.iter_mut() {
                columns::set_rows(chunk, 0);
            }
            mask.fill(u64::MAX);
            rows.fill(0);
            RowMask::try_new(ROWS, &mut mask)
                .ok()
                .and_then(|mut pending| {
                    append_partitioned(
                        chunks.as_mut_slice(),
                        &partitions,
                        black_box(&batch.hashes),
                        &columns,
                        &mut pending,
                        &mut offsets,
                        &mut rows,
                    )
                    .ok()
                })
        })?;
        group.op("reference", || plain.append(black_box(&batch), 3))?;
    }
    Ok(())
}

/// A block's header written and read, against its fields laid out at
/// their offsets with no check.
fn measure_header(runner: &mut Runner) -> Result<()> {
    const MAX_LEN: u64 = 1 << 30;
    let header = BlockHeader {
        kind: BlockKind::Columns,
        number: 7,
        partition: 3,
        level: 1,
        fingerprint: 0x0123_4567_89AB_CDEF,
        len: 4096,
        packed: 1024,
    };
    let mut bytes = [0_u8; HEADER_SIZE];
    header.write(&mut bytes, MAX_LEN)?;
    ensure!(
        BlockHeader::read(&bytes, header.fingerprint, MAX_LEN)? == header,
        "the header read back differs"
    );
    let plain = |header: &BlockHeader, out: &mut [u8; HEADER_SIZE]| {
        out[..8].copy_from_slice(&relspill::MAGIC.to_ne_bytes());
        out[8..12].copy_from_slice(&relspill::VERSION.to_ne_bytes());
        out[12..16].copy_from_slice(&(header.kind as u32).to_ne_bytes());
        out[16..20].copy_from_slice(&header.number.to_ne_bytes());
        out[20..24].copy_from_slice(&header.partition.to_ne_bytes());
        out[24..28].copy_from_slice(&header.level.to_ne_bytes());
        out[28..32].copy_from_slice(&header.packed.to_ne_bytes());
        out[32..40].copy_from_slice(&header.fingerprint.to_ne_bytes());
        out[40..48].copy_from_slice(&header.len.to_ne_bytes());
    };
    let mut laid = [0_u8; HEADER_SIZE];
    plain(&header, &mut laid);
    ensure!(laid == bytes, "the plain layout differs from the header's");
    let mut out = [0_u8; HEADER_SIZE];
    let mut group = runner.group("spill/header/columns");
    group.op("write", || {
        black_box(&header).write(&mut out, MAX_LEN).is_ok()
    })?;
    group.op("read", || {
        BlockHeader::read(black_box(&bytes), header.fingerprint, MAX_LEN).is_ok()
    })?;
    group.op("reference", || {
        plain(black_box(&header), &mut out);
        out[0]
    })?;
    Ok(())
}
