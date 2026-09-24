//! Measured probes of a table far larger than the caches.
//!
//! The table of [`crate::tabling`] fits in L1, where a probe waits for
//! nothing; a join's build side does not. Here one table of `RECORDS`
//! records (one int4 key, an 8-byte payload: 128 MiB of records and 32
//! MiB of buckets in a release build) is built once per process, outside
//! the counters, and probed by batches of 1024 rows taken in turn from a
//! ring of `BATCHES` batches of distinct keys, so that the records and
//! buckets a call touches come from memory, not from the caches.
//!
//! The counters demand the same instructions from every call, so the
//! work of a batch must not depend on its keys: the table's keys are
//! chosen so that each has a bucket of its own (every chain is one
//! record long), and the absent keys fall into empty buckets. `probe_hit`
//! and `find_or_insert` look present keys up (the latter creates
//! nothing), `probe_miss` absent ones, each against `reference`, a chained
//! table of the same shape with plain stores built from the same keys.
//! A debug build, which only checks the model, uses a small table.

use anyhow::{Result, ensure};
use tessera_core::{ColumnView, RowMask, RowMaskView};
use tessera_kernels::int32::murmurhash32;
use tessera_kernels::table::{KeyKind, TableConfig, TableMut, region_size};

use crate::support::runner::Runner;

const CONFIG: TableConfig<'static> = TableConfig {
    keys: &[KeyKind::Int32],
    payload_size: 8,
};

/// Rows of a probe batch.
pub const BATCH: usize = 1024;

/// Records of the table: 2^22 when measured, 2^14 in a check-only build.
pub const RECORDS: usize = if cfg!(debug_assertions) {
    1 << 14
} else {
    1 << 22
};

/// The table's buckets, as the format chooses them for `RECORDS`.
const BUCKETS: usize = RECORDS * 2;

/// Batches of the probe ring: every present key once.
pub const BATCHES: usize = RECORDS / BATCH;

/// The bucket of a hash in a table of `BUCKETS` buckets.
fn bucket(hash: u32) -> usize {
    (hash >> (32 - BUCKETS.trailing_zeros())) as usize
}

fn random(state: &mut u64) -> u64 {
    *state ^= *state >> 12;
    *state ^= *state << 25;
    *state ^= *state >> 27;
    state.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

/// The keys: `present` with a bucket each, in random order, and as many
/// `absent` keys whose buckets are empty.
pub struct Keys {
    pub present: Vec<i32>,
    pub absent: Vec<i32>,
}

impl Keys {
    pub fn new() -> Self {
        let mut used = vec![false; BUCKETS];
        let mut present = Vec::with_capacity(RECORDS);
        let mut candidate = 0_i32;
        while present.len() < RECORDS {
            let slot = &mut used[bucket(murmurhash32(candidate as u32))];
            if !*slot {
                *slot = true;
                present.push(candidate);
            }
            candidate += 1;
        }
        let mut absent = Vec::with_capacity(RECORDS);
        while absent.len() < RECORDS {
            if !used[bucket(murmurhash32(candidate as u32))] {
                absent.push(candidate);
            }
            candidate += 1;
        }
        let mut state = 0x9e37_79b9_7f4a_7c15;
        for keys in [&mut present, &mut absent] {
            for i in (1..keys.len()).rev() {
                keys.swap(i, (random(&mut state) % (i as u64 + 1)) as usize);
            }
        }
        Self { present, absent }
    }
}

/// A record of the reference table.
#[derive(Clone, Copy, Default)]
struct Entry {
    hash: u32,
    next: u32,
    key: i64,
    payload: [u8; 8],
}

/// The reference: a chained table on vectors with plain stores, as in
/// [`crate::tabling`], without NULL handling (the keys have none).
pub struct Reference {
    buckets: Vec<u32>,
    shift: u32,
    records: Vec<Entry>,
}

impl Reference {
    fn new(keys: &[i32]) -> Self {
        let mut reference = Self {
            buckets: vec![0; BUCKETS],
            shift: 32 - BUCKETS.trailing_zeros(),
            records: Vec::with_capacity(keys.len()),
        };
        for &key in keys {
            let hash = murmurhash32(key as u32);
            let bucket = (hash >> reference.shift) as usize;
            reference.records.push(Entry {
                hash,
                next: reference.buckets[bucket],
                key: i64::from(key),
                payload: [0; 8],
            });
            reference.buckets[bucket] = reference.records.len() as u32;
        }
        reference
    }

    /// Probe a batch; the count found.
    #[inline(never)]
    pub fn probe(&self, hashes: &[u32], keys: &[i32]) -> usize {
        let mut found = 0;
        for (&hash, &key) in hashes.iter().zip(keys) {
            let mut index = self.buckets[(hash >> self.shift) as usize];
            while index != 0 {
                let entry = &self.records[index as usize - 1];
                if entry.hash == hash && entry.key == i64::from(key) {
                    found += usize::from(entry.payload[0] == 0);
                    break;
                }
                index = entry.next;
            }
        }
        found
    }
}

/// The table, its keys with their hashes, and the output buffers.
pub struct Setup {
    pub words: Vec<u64>,
    pub keys: Keys,
    pub present_hashes: Vec<u32>,
    pub absent_hashes: Vec<u32>,
    pub reference: Reference,
    pub all: [u64; BATCH / 64],
    pub found: [u64; BATCH / 64],
    pub inserted: [u64; BATCH / 64],
    pub pending: [u64; BATCH / 64],
    pub matches: Vec<u32>,
}

impl Setup {
    pub fn new() -> Result<Self> {
        let keys = Keys::new();
        let hashes =
            |keys: &[i32]| -> Vec<u32> { keys.iter().map(|&k| murmurhash32(k as u32)).collect() };
        let present_hashes = hashes(&keys.present);
        let absent_hashes = hashes(&keys.absent);
        let mut words = vec![0; region_size(&CONFIG, RECORDS as u64)?.div_ceil(8)];
        {
            let table = TableMut::create_in(&mut words, &CONFIG, RECORDS as u64)?;
            ensure!(
                table.stats().buckets == BUCKETS as u64,
                "unexpected bucket count"
            );
            let mut offsets = vec![0; BATCH];
            for batch in 0..BATCHES {
                let rows = batch * BATCH..(batch + 1) * BATCH;
                let column = [ColumnView::try_new(&keys.present[rows.clone()], None)?];
                let mut pending_words = [u64::MAX; BATCH / 64];
                let mut pending = RowMask::try_new(BATCH, &mut pending_words)?;
                table.insert(
                    &present_hashes[rows],
                    &column[..],
                    None,
                    &mut pending,
                    &mut offsets,
                )?;
                ensure!(
                    pending_words == [0; BATCH / 64],
                    "the table ran out of room"
                );
            }
        }
        let reference = Reference::new(&keys.present);
        Ok(Self {
            words,
            keys,
            present_hashes,
            absent_hashes,
            reference,
            all: [u64::MAX; BATCH / 64],
            found: [0; BATCH / 64],
            inserted: [0; BATCH / 64],
            pending: [0; BATCH / 64],
            matches: vec![0; BATCH],
        })
    }
}

/// Probe batch `batch` of present or absent keys.
#[inline(never)]
pub fn probe(setup: &mut Setup, batch: usize, present: bool) -> Result<()> {
    let rows = batch * BATCH..(batch + 1) * BATCH;
    let (keys, hashes) = if present {
        (
            &setup.keys.present[rows.clone()],
            &setup.present_hashes[rows],
        )
    } else {
        (&setup.keys.absent[rows.clone()], &setup.absent_hashes[rows])
    };
    let column = [ColumnView::try_new(keys, None)?];
    let table = TableMut::exclusive(&mut setup.words)?;
    let all = RowMaskView::try_new(BATCH, &setup.all)?;
    let mut found = RowMask::try_new(BATCH, &mut setup.found)?;
    table.probe(hashes, &column[..], &all, &mut setup.matches, &mut found)
}

/// Resolve batch `batch` of present keys to their records.
#[inline(never)]
pub fn resolve(setup: &mut Setup, batch: usize) -> Result<()> {
    let rows = batch * BATCH..(batch + 1) * BATCH;
    let column = [ColumnView::try_new(
        &setup.keys.present[rows.clone()],
        None,
    )?];
    let mut table = TableMut::exclusive(&mut setup.words)?;
    setup.pending = setup.all;
    let mut pending = RowMask::try_new(BATCH, &mut setup.pending)?;
    let mut inserted = RowMask::try_new(BATCH, &mut setup.inserted)?;
    table.find_or_insert(
        &setup.present_hashes[rows],
        &column[..],
        &mut pending,
        &mut setup.matches,
        &mut inserted,
    )?;
    Ok(())
}

/// Check every operation over every batch against the model: present
/// keys are found at a record with their key and nothing is created,
/// absent keys are not found, and the reference agrees.
pub fn check(setup: &mut Setup) -> Result<()> {
    for batch in 0..BATCHES {
        probe(setup, batch, true)?;
        ensure!(
            setup.found == [u64::MAX; BATCH / 64],
            "batch {batch}: a present key missed"
        );
        {
            let table = TableMut::exclusive(&mut setup.words)?;
            for (row, &offset) in setup.matches.iter().enumerate() {
                let key = setup.keys.present[batch * BATCH + row];
                ensure!(
                    table.record(offset)?.keys == [i64::from(key)],
                    "batch {batch}: row {row} found another key"
                );
            }
        }
        resolve(setup, batch)?;
        ensure!(
            setup.pending == [0; BATCH / 64] && setup.inserted == [0; BATCH / 64],
            "batch {batch}: find_or_insert created a record"
        );
        probe(setup, batch, false)?;
        ensure!(
            setup.found == [0; BATCH / 64],
            "batch {batch}: an absent key hit"
        );
        let rows = batch * BATCH..(batch + 1) * BATCH;
        ensure!(
            setup.reference.probe(
                &setup.present_hashes[rows.clone()],
                &setup.keys.present[rows.clone()]
            ) == BATCH,
            "batch {batch}: the reference missed a present key"
        );
        ensure!(
            setup
                .reference
                .probe(&setup.absent_hashes[rows.clone()], &setup.keys.absent[rows])
                == 0,
            "batch {batch}: the reference hit an absent key"
        );
    }
    let table = TableMut::exclusive(&mut setup.words)?;
    ensure!(
        table.stats().records == RECORDS as u64,
        "records were created"
    );
    Ok(())
}

/// The next batch of the ring after `cursor`.
fn advance(cursor: &mut usize) -> usize {
    let batch = *cursor;
    *cursor = if batch + 1 == BATCHES { 0 } else { batch + 1 };
    batch
}

pub fn bench(runner: &mut Runner) -> Result<()> {
    use std::hint::black_box;
    let mut setup = Setup::new()?;
    check(&mut setup)?;
    let mut cursor = 0;
    let mut group = runner.group(format!("table_large/dense/{RECORDS}/hit"));
    group.op("probe_hit", || {
        let batch = advance(&mut cursor);
        probe(black_box(&mut setup), batch, true).unwrap()
    })?;
    group.op("find_or_insert", || {
        let batch = advance(&mut cursor);
        resolve(black_box(&mut setup), batch).unwrap()
    })?;
    group.op("reference", || {
        let batch = advance(&mut cursor);
        let rows = batch * BATCH..(batch + 1) * BATCH;
        let setup = black_box(&setup);
        setup.reference.probe(
            &setup.present_hashes[rows.clone()],
            &setup.keys.present[rows],
        )
    })?;
    let mut group = runner.group(format!("table_large/dense/{RECORDS}/miss"));
    group.op("probe_miss", || {
        let batch = advance(&mut cursor);
        probe(black_box(&mut setup), batch, false).unwrap()
    })?;
    group.op("reference", || {
        let batch = advance(&mut cursor);
        let rows = batch * BATCH..(batch + 1) * BATCH;
        let setup = black_box(&setup);
        setup
            .reference
            .probe(&setup.absent_hashes[rows.clone()], &setup.keys.absent[rows])
    })?;
    Ok(())
}
