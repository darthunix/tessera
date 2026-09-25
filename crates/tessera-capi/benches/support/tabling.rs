//! Measured table operations over the reading fixtures.
//!
//! One int4 key per row, NULL as a group key so that every selected row
//! is valid, and an 8-byte payload of zeros. The table lives in a region
//! of words sized for the case: `insert` recreates it and inserts every
//! selected row, `probe_hit` finds every row, `probe_miss` probes with
//! hashes of absent keys, and `find_or_insert` resolves every row to its
//! record without creating one. The reference is a chained table of the
//! same shape with plain stores, built and probed in the benchmark itself
//! straight from the fixture buffers: each case has an `insert` group,
//! where the reference builds that table, and a `probe` group, where it
//! looks every row up. Results are checked against a model before
//! counting.

use anyhow::{Result, ensure};
use tessera_core::{ColumnReader, RowMask, RowMaskView};
use tessera_kernels::int32::{self, NullKeys};
use tessera_kernels::table::{
    KeyKind, KeySource, TableConfig, TableMut, normalize_word, region_size,
};

use crate::reading::Input;
use crate::support::{fixture::Fixture, runner::Runner};

const CONFIG: TableConfig<'static> = TableConfig {
    keys: &[KeyKind::Int32],
    payload_size: 8,
};

/// The one key column of a batch.
pub struct OneKey<'a, C>(pub &'a C);

impl<C: ColumnReader<Value = i32>> KeySource for OneKey<'_, C> {
    fn nkeys(&self) -> usize {
        1
    }

    fn nrows(&self) -> usize {
        self.0.nrows()
    }

    fn word(&self, _: usize, index: usize, selected: u64, out: &mut [i64; 64]) -> Result<u64> {
        normalize_word(self.0, index, selected, out)
    }
}

/// The buffers of one case: the hashes of the batch and of absent keys,
/// the valid mask, the region and the outputs.
pub struct Setup {
    pub hashes: Vec<u32>,
    pub absent: Vec<u32>,
    pub valid: Vec<u64>,
    pub words: Vec<u64>,
    pub pending: Vec<u64>,
    pub found: Vec<u64>,
    pub inserted: Vec<u64>,
    pub offsets: Vec<u32>,
    pub matches: Vec<u32>,
}

impl Setup {
    pub fn new<C: ColumnReader<Value = i32>>(input: &Input<'_, C>) -> Result<Self> {
        let nrows = input.rows.nrows();
        let words = nrows.div_ceil(64);
        let mut hashes = vec![0; nrows];
        let mut valid: Vec<u64> = (0..words)
            .map(|index| input.rows.word(index).unwrap())
            .collect();
        let mut mask = RowMask::try_new(nrows, &mut valid)?;
        int32::hash(
            input.column,
            &input.rows,
            NullKeys::Group,
            &mut hashes,
            &mut mask,
        )?;
        // Hashes of keys nobody inserted: other buckets, other chains.
        let absent = hashes.iter().map(|hash| hash ^ 0x5555_5555).collect();
        let region = region_size(&CONFIG, nrows as u64)?;
        Ok(Self {
            hashes,
            absent,
            valid,
            words: vec![0; region / 8],
            pending: vec![0; words],
            found: vec![0; words],
            inserted: vec![0; words],
            offsets: vec![0; nrows],
            matches: vec![0; nrows],
        })
    }
}

/// Recreate the table and insert every valid row.
#[inline(never)]
pub fn insert_all<K: KeySource>(setup: &mut Setup, keys: &K) -> Result<()> {
    let nrows = setup.hashes.len();
    let table = TableMut::create_in(&mut setup.words, &CONFIG, nrows as u64)?;
    setup.pending.copy_from_slice(&setup.valid);
    let mut pending = RowMask::try_new(nrows, &mut setup.pending)?;
    table.insert(&setup.hashes, keys, None, &mut pending, &mut setup.offsets)?;
    Ok(())
}

/// Probe every valid row with the given hashes.
#[inline(never)]
pub fn probe_all<K: KeySource>(setup: &mut Setup, keys: &K, hashes: &[u32]) -> Result<()> {
    let nrows = setup.hashes.len();
    let table = TableMut::exclusive(&mut setup.words)?;
    let rows = RowMaskView::try_new(nrows, &setup.valid)?;
    let mut found = RowMask::try_new(nrows, &mut setup.found)?;
    table.probe(hashes, keys, &rows, &mut setup.matches, &mut found)
}

/// Resolve every valid row to its record.
#[inline(never)]
pub fn resolve_all<K: KeySource>(setup: &mut Setup, keys: &K) -> Result<()> {
    let nrows = setup.hashes.len();
    let mut table = TableMut::exclusive(&mut setup.words)?;
    setup.pending.copy_from_slice(&setup.valid);
    let mut pending = RowMask::try_new(nrows, &mut setup.pending)?;
    let mut inserted = RowMask::try_new(nrows, &mut setup.inserted)?;
    table.find_or_insert(
        &setup.hashes,
        keys,
        &mut pending,
        &mut setup.offsets,
        &mut inserted,
    )?;
    Ok(())
}

/// Add one to the first payload word of every valid row's record, the
/// offsets of the last resolution: `count(*)` of a grouped aggregate.
#[inline(never)]
pub fn count_all(setup: &mut Setup) -> Result<()> {
    let nrows = setup.hashes.len();
    let mut table = TableMut::exclusive(&mut setup.words)?;
    let rows = RowMaskView::try_new(nrows, &setup.valid)?;
    table.count_rows(&setup.offsets, &rows, 0)
}

/// The same scatter with plain stores: a counter per record offset.
#[inline(never)]
pub fn reference_count(counts: &mut [u64], setup: &Setup) {
    for (index, &word) in setup.valid.iter().enumerate() {
        let mut bits = word;
        while bits != 0 {
            let row = index * 64 + bits.trailing_zeros() as usize;
            bits &= bits - 1;
            counts[setup.offsets[row] as usize] += 1;
        }
    }
}

/// A record of the reference table.
#[derive(Clone, Copy, Default)]
struct Entry {
    hash: u32,
    next: u32,
    null: bool,
    key: i64,
    payload: [u8; 8],
}

/// The reference: buckets of the high bits of the hash holding 1-based
/// indexes into a record vector, chained through `next`, with plain
/// stores and no checks beyond the vector's own.
pub struct Reference {
    buckets: Vec<u32>,
    shift: u32,
    records: Vec<Entry>,
}

impl Reference {
    pub fn new(nrows: usize) -> Self {
        let nbuckets = (nrows * 2).max(1024).next_power_of_two();
        Self {
            buckets: vec![0; nbuckets],
            shift: 32 - nbuckets.trailing_zeros(),
            records: Vec::with_capacity(nrows),
        }
    }

    fn clear(&mut self) {
        self.buckets.fill(0);
        self.records.clear();
    }

    fn insert(&mut self, hash: u32, null: bool, key: i64) {
        let bucket = (hash >> self.shift) as usize;
        let index = self.records.len() as u32 + 1;
        self.records.push(Entry {
            hash,
            next: self.buckets[bucket],
            null,
            key,
            payload: [0; 8],
        });
        self.buckets[bucket] = index;
    }

    /// The payload of the newest record with the key.
    fn probe(&self, hash: u32, null: bool, key: i64) -> Option<&[u8; 8]> {
        let mut index = self.buckets[(hash >> self.shift) as usize];
        while index != 0 {
            let entry = &self.records[index as usize - 1];
            if entry.hash == hash && entry.null == null && entry.key == key {
                return Some(&entry.payload);
            }
            index = entry.next;
        }
        None
    }
}

/// The key of a row straight from the fixture buffers: NULL and value.
pub type KeyOf<'a> = &'a dyn Fn(usize) -> (bool, i64);

/// Clear the reference and insert every valid row.
#[inline(never)]
pub fn reference_insert(reference: &mut Reference, setup: &Setup, key_of: KeyOf<'_>) {
    reference.clear();
    for (index, &word) in setup.valid.iter().enumerate() {
        let mut bits = word;
        while bits != 0 {
            let row = index * 64 + bits.trailing_zeros() as usize;
            bits &= bits - 1;
            let (null, key) = key_of(row);
            reference.insert(setup.hashes[row], null, key);
        }
    }
}

/// Probe every valid row of the reference with the given hashes; the
/// count found.
#[inline(never)]
pub fn reference_probe(
    reference: &Reference,
    setup: &Setup,
    hashes: &[u32],
    key_of: KeyOf<'_>,
) -> usize {
    let mut found = 0;
    for (index, &word) in setup.valid.iter().enumerate() {
        let mut bits = word;
        while bits != 0 {
            let row = index * 64 + bits.trailing_zeros() as usize;
            bits &= bits - 1;
            let (null, key) = key_of(row);
            found += usize::from(reference.probe(hashes[row], null, key).is_some());
        }
    }
    found
}

/// Check every operation against the model before anything is measured:
/// every valid row gets a record with its key, is found at one, and misses
/// under absent hashes.
pub fn check<K: KeySource>(
    setup: &mut Setup,
    keys: &K,
    reference: &mut Reference,
    key_of: KeyOf<'_>,
) -> Result<()> {
    let nrows = setup.hashes.len();
    let valid_rows: Vec<usize> = RowMaskView::try_new(nrows, &setup.valid)?
        .selected_indices()
        .collect();
    insert_all(setup, keys)?;
    ensure!(
        setup.pending.iter().all(|&word| word == 0),
        "rows without room"
    );
    let offsets = setup.offsets.clone();
    {
        let table = TableMut::exclusive(&mut setup.words)?;
        ensure!(
            table.stats().records == valid_rows.len() as u64,
            "record count differs from the valid rows"
        );
        for &row in &valid_rows {
            let record = table.record(offsets[row])?;
            let (null, key) = key_of(row);
            ensure!(
                record.null_bits == u32::from(null) && record.keys == [key],
                "row {row} has another key in its record"
            );
        }
    }
    let absent = setup.absent.clone();
    let hashes = setup.hashes.clone();
    probe_all(setup, keys, &hashes)?;
    ensure!(setup.found == setup.valid, "a valid row is not found");
    for &row in &valid_rows {
        let (null, key) = key_of(row);
        let table = TableMut::exclusive(&mut setup.words)?;
        let record = table.record(setup.matches[row])?;
        ensure!(
            record.null_bits == u32::from(null) && record.keys == [key],
            "row {row} matches another key"
        );
    }
    let matches = setup.matches.clone();
    probe_all(setup, keys, &absent)?;
    ensure!(
        setup.found.iter().all(|&word| word == 0),
        "an absent hash hits"
    );
    resolve_all(setup, keys)?;
    ensure!(
        setup.pending.iter().all(|&word| word == 0)
            && setup.inserted.iter().all(|&word| word == 0)
            && setup.offsets == matches,
        "a resolved row differs from its probe"
    );
    reference_insert(reference, setup, key_of);
    ensure!(
        reference_probe(reference, setup, &hashes, key_of) == valid_rows.len(),
        "the reference misses a row"
    );
    ensure!(
        reference_probe(reference, setup, &absent, key_of) == 0,
        "the reference hits an absent hash"
    );
    Ok(())
}

pub fn bench(runner: &mut Runner) -> Result<()> {
    for case in crate::reading::cases() {
        let dense = case.dense_column()?;
        let input = Input::new(&dense, &case);
        let non_nulls = input.non_nulls;
        let values = input.values;
        measure_column(runner, "dense", &input, &case, &|row| {
            let null = non_nulls.is_some_and(|mask| !mask.contains(row));
            (null, if null { 0 } else { i64::from(values[row]) })
        })?;
        let datum = case.datum_column()?;
        let input = Input::new(&datum, &case);
        let (datums, nulls) = (input.datums, input.nulls);
        measure_column(runner, "datum", &input, &case, &|row| {
            let null = nulls[row];
            (
                null,
                if null {
                    0
                } else {
                    i64::from(datums[row] as i32)
                },
            )
        })?;
    }
    Ok(())
}

fn measure_column<C: ColumnReader<Value = i32>>(
    runner: &mut Runner,
    format: &str,
    input: &Input<'_, C>,
    case: &Fixture,
    key_of: KeyOf<'_>,
) -> Result<()> {
    use std::hint::black_box;
    let keys = OneKey(input.column);
    let mut setup = Setup::new(input)?;
    let mut reference = Reference::new(setup.hashes.len());
    check(&mut setup, &keys, &mut reference, key_of)?;
    let hashes = setup.hashes.clone();
    let absent = setup.absent.clone();
    // Two groups per case, each with the reference its operations compare
    // with: building a table, and looking rows up in a built one.
    let mut group = runner.group(format!("table_int32/{format}/{}/insert", case.name));
    group.op("insert", || {
        insert_all(&mut setup, black_box(&keys)).unwrap()
    })?;
    group.op("reference", || {
        reference_insert(&mut reference, black_box(&setup), key_of)
    })?;
    let mut group = runner.group(format!("table_int32/{format}/{}/probe", case.name));
    group.op("probe_hit", || {
        probe_all(&mut setup, black_box(&keys), &hashes).unwrap()
    })?;
    group.op("probe_miss", || {
        probe_all(&mut setup, black_box(&keys), &absent).unwrap()
    })?;
    group.op("find_or_insert", || {
        resolve_all(&mut setup, black_box(&keys)).unwrap()
    })?;
    group.op("reference", || {
        reference_probe(&reference, black_box(&setup), &hashes, key_of)
    })?;
    // Grouping: count(*) into the records the resolution gave the rows,
    // against the same scatter into a plain array by offset.
    resolve_all(&mut setup, &keys)?;
    let mut counts = vec![0_u64; setup.words.len()];
    group.op("count_rows", || count_all(black_box(&mut setup)).unwrap())?;
    group.op("reference_count", || {
        reference_count(&mut counts, black_box(&setup))
    })?;
    Ok(())
}
