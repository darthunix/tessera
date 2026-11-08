//! Measured table operations over the reading fixtures.
//!
//! One int4 key per row, NULL as a group key so that every selected row
//! is valid, and an 8-byte payload of zeros. The table owns an index sized
//! for the case and chunks of 64 KiB: `insert` empties it and inserts every
//! selected row, `probe_hit` finds every row, `probe_miss` probes with
//! hashes of absent keys, and `find_or_insert` resolves every row to its
//! record without creating one. The reference is a chained table of the
//! same shape with plain stores, built and probed in the benchmark itself
//! straight from the fixture buffers: each case has an `insert` group,
//! where the reference builds that table, and a `probe` group, where it
//! looks every row up. Results are checked against a model before
//! counting. A case drawn at random takes a ring of inputs, which hold the
//! same keys in other rows, so one table serves them all. The inputs of a
//! density case select other rows of the same values: the table that the
//! probes take holds the keys of every one, as a table of a join's inner
//! side serves batches of any selection.

use anyhow::{Result, ensure};
use tessera_core::{ColumnReader, RowMask, RowMaskView};
use tessera_kernels::accumulate;
use tessera_kernels::column_keys::normalize_word;
use tessera_kernels::int32::{self, NullKeys};
use tessera_kernels::table::{Batch, KeySource, Matches, OwnedTable, TableConfig};

use crate::reading::Input;
use crate::support::{
    fixture::{self, Fixture},
    runner::Runner,
};

const CONFIG: TableConfig = TableConfig {
    nkeys: 1,
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

    fn word(&self, _: usize, index: usize, selected: u64, out: &mut [i64; 64]) -> Option<u64> {
        normalize_word(self.0, index, selected, out)
    }
}

/// The table of a case, sized for one input and its chunks of 64 KiB.
pub fn table(nrows: usize) -> Result<OwnedTable> {
    Ok(OwnedTable::new(&CONFIG, nrows as u64, 64 << 10)?)
}

/// The buffers of one input: the hashes of the batch and of absent keys,
/// the valid mask and the outputs.
pub struct Setup {
    pub hashes: Vec<u32>,
    pub absent: Vec<u32>,
    pub valid: Vec<u64>,
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
            .map(|index| input.rows.get_word(index).unwrap())
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
        Ok(Self {
            hashes,
            absent,
            valid,
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
pub fn insert_all<K: KeySource>(table: &mut OwnedTable, setup: &mut Setup, keys: &K) -> Result<()> {
    let nrows = setup.hashes.len();
    table.reset()?;
    table.insert(
        &setup.hashes,
        keys,
        RowMaskView::try_new(nrows, &setup.valid)?,
        None,
        &mut setup.offsets,
    )?;
    Ok(())
}

/// Probe every valid row with the given hashes.
#[inline(never)]
pub fn probe_all<K: KeySource>(
    table: &OwnedTable,
    setup: &mut Setup,
    keys: &K,
    hashes: &[u32],
) -> Result<()> {
    let table = table.table()?;
    setup.found.copy_from_slice(&setup.valid);
    let mut matches = Matches::new(&mut setup.found, &mut setup.matches)?;
    table.probe(hashes, keys, &mut matches)?;
    Ok(())
}

/// Resolve every valid row to its record.
#[inline(never)]
pub fn resolve_all<K: KeySource>(
    table: &mut OwnedTable,
    setup: &mut Setup,
    keys: &K,
) -> Result<()> {
    let nrows = setup.hashes.len();
    setup.pending.copy_from_slice(&setup.valid);
    // The call adds the rows it creates: a caller clears them once a batch.
    setup.inserted.fill(0);
    let mut inserted = RowMask::try_new(nrows, &mut setup.inserted)?;
    let chunk = table.chunks() - 1;
    table.table_mut()?.find_or_insert(
        chunk,
        &mut Batch::new(&setup.hashes, keys, &mut setup.pending, &mut setup.offsets)?,
        &mut inserted,
    )?;
    Ok(())
}

/// Add one to the first payload word of every valid row's record, the
/// offsets of the last resolution: `count(*)` of a grouped aggregate.
#[inline(never)]
pub fn count_all(table: &mut OwnedTable, setup: &Setup) -> Result<()> {
    let nrows = setup.hashes.len();
    let mut table = table.table_mut()?;
    let rows = RowMaskView::try_new(nrows, &setup.valid)?;
    accumulate::count_rows(&mut table, &setup.offsets, &rows, 0)
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

/// Insert into the reference the valid rows whose keys it does not hold.
fn reference_add(reference: &mut Reference, setup: &Setup, key_of: KeyOf<'_>) {
    for (index, &word) in setup.valid.iter().enumerate() {
        let mut bits = word;
        while bits != 0 {
            let row = index * 64 + bits.trailing_zeros() as usize;
            bits &= bits - 1;
            let (null, key) = key_of(row);
            if reference.probe(setup.hashes[row], null, key).is_none() {
                reference.insert(setup.hashes[row], null, key);
            }
        }
    }
}

/// Give the table and the reference the keys of every input that they do
/// not hold: none for the inputs of a ring, which hold the same keys, and
/// those of the other selections for a density case.
fn hold_every_input<K: KeySource>(
    table: &mut OwnedTable,
    reference: &mut Reference,
    setups: &mut [Setup],
    keys: &[K],
    key_of: &[KeyOf<'_>],
) -> Result<()> {
    for ((setup, keys), key_of) in setups.iter_mut().zip(keys).zip(key_of) {
        resolve_all(table, setup, keys)?;
        ensure!(
            setup.pending.iter().all(|&word| word == 0),
            "rows without room"
        );
        reference_add(reference, setup, *key_of);
    }
    Ok(())
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
    table: &mut OwnedTable,
    setup: &mut Setup,
    keys: &K,
    reference: &mut Reference,
    key_of: KeyOf<'_>,
) -> Result<()> {
    let nrows = setup.hashes.len();
    let valid_rows: Vec<usize> = RowMaskView::try_new(nrows, &setup.valid)?
        .selected_rows()
        .collect();
    insert_all(table, setup, keys)?;
    ensure!(
        setup.pending.iter().all(|&word| word == 0),
        "rows without room"
    );
    let offsets = setup.offsets.clone();
    {
        let table = table.table()?;
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
    probe_all(table, setup, keys, &hashes)?;
    ensure!(setup.found == setup.valid, "a valid row is not found");
    for &row in &valid_rows {
        let (null, key) = key_of(row);
        let table = table.table()?;
        let record = table.record(setup.matches[row])?;
        ensure!(
            record.null_bits == u32::from(null) && record.keys == [key],
            "row {row} matches another key"
        );
    }
    let matches = setup.matches.clone();
    probe_all(table, setup, keys, &absent)?;
    ensure!(
        setup.found.iter().all(|&word| word == 0),
        "an absent hash hits"
    );
    resolve_all(table, setup, keys)?;
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
        if !runner.takes(&case.name) {
            continue;
        }
        let cases = case.inputs();
        let dense = fixture::columns(&cases, Fixture::dense_column)?;
        let inputs = Input::each(&dense, &cases);
        let keys: Vec<_> = inputs
            .iter()
            .map(|input| {
                let (non_nulls, values) = (input.non_nulls, input.values);
                move |row: usize| {
                    let null = non_nulls.is_some_and(|mask| !mask.contains(row));
                    (null, if null { 0 } else { i64::from(values[row]) })
                }
            })
            .collect();
        let key_of: Vec<KeyOf<'_>> = keys.iter().map(|key| key as KeyOf<'_>).collect();
        measure_column(runner, "dense", &inputs, &cases[0].name, &key_of)?;
        let datum = fixture::columns(&cases, Fixture::datum_column)?;
        let inputs = Input::each(&datum, &cases);
        let keys: Vec<_> = inputs
            .iter()
            .map(|input| {
                let (datums, nulls) = (input.datums, input.nulls);
                move |row: usize| {
                    let null = nulls[row];
                    (
                        null,
                        if null {
                            0
                        } else {
                            i64::from(datums[row] as i32)
                        },
                    )
                }
            })
            .collect();
        let key_of: Vec<KeyOf<'_>> = keys.iter().map(|key| key as KeyOf<'_>).collect();
        measure_column(runner, "datum", &inputs, &cases[0].name, &key_of)?;
    }
    Ok(())
}

/// The operations over `inputs` taken in turn, one table for them all.
fn measure_column<C: ColumnReader<Value = i32>>(
    runner: &mut Runner,
    format: &str,
    inputs: &[Input<'_, C>],
    name: &str,
    key_of: &[KeyOf<'_>],
) -> Result<()> {
    use std::hint::black_box;
    let keys: Vec<_> = inputs.iter().map(|input| OneKey(input.column)).collect();
    let mut setups = inputs.iter().map(Setup::new).collect::<Result<Vec<_>>>()?;
    let mut table = table(inputs[0].rows.nrows())?;
    let mut reference = Reference::new(setups[0].hashes.len());
    for ((setup, keys), key_of) in setups.iter_mut().zip(&keys).zip(key_of) {
        check(&mut table, setup, keys, &mut reference, *key_of)?;
    }
    let hashes: Vec<_> = setups.iter().map(|setup| setup.hashes.clone()).collect();
    let absent: Vec<_> = setups.iter().map(|setup| setup.absent.clone()).collect();
    hold_every_input(&mut table, &mut reference, &mut setups, &keys, key_of)?;
    for ((setup, keys), hashes) in setups.iter_mut().zip(&keys).zip(&hashes) {
        probe_all(&table, setup, keys, hashes)?;
        ensure!(
            setup.found == setup.valid,
            "a row is not in another input's table"
        );
        resolve_all(&mut table, setup, keys)?;
        ensure!(
            setup.inserted.iter().all(|&word| word == 0),
            "a row is not in another input's table"
        );
    }
    let period = inputs.len();
    // Two groups per case, each with the reference its operations compare
    // with: building a table, and looking rows up in a built one, which
    // holds the keys of every input.
    let mut group = runner.group(format!("table_int32/{format}/{name}/insert"));
    group.op_turns("insert", period, |at| {
        insert_all(&mut table, &mut setups[at], black_box(&keys[at])).unwrap()
    })?;
    group.op_turns("reference", period, |at| {
        reference_insert(&mut reference, black_box(&setups[at]), key_of[at])
    })?;
    hold_every_input(&mut table, &mut reference, &mut setups, &keys, key_of)?;
    let mut group = runner.group(format!("table_int32/{format}/{name}/probe"));
    group.op_turns("probe_hit", period, |at| {
        probe_all(&table, &mut setups[at], black_box(&keys[at]), &hashes[at]).unwrap()
    })?;
    group.op_turns("probe_miss", period, |at| {
        probe_all(&table, &mut setups[at], black_box(&keys[at]), &absent[at]).unwrap()
    })?;
    group.op_turns("find_or_insert", period, |at| {
        resolve_all(&mut table, &mut setups[at], black_box(&keys[at])).unwrap()
    })?;
    group.op_turns("reference", period, |at| {
        reference_probe(&reference, black_box(&setups[at]), &hashes[at], key_of[at])
    })?;
    // Grouping: count(*) into the records the resolution gave the rows,
    // against the same scatter into a plain array by offset.
    for (setup, keys) in setups.iter_mut().zip(&keys) {
        resolve_all(&mut table, setup, keys)?;
    }
    let mut counts = vec![0_u64; table.chunks() << 17];
    group.op_turns("count_rows", period, |at| {
        count_all(&mut table, black_box(&setups[at])).unwrap()
    })?;
    group.op_turns("reference_count", period, |at| {
        reference_count(&mut counts, black_box(&setups[at]))
    })?;
    Ok(())
}
