//! The owning table under any order of its calls, against a model of the
//! records each key has.

use std::collections::BTreeMap;

use anyhow::{Result, ensure};
use proptest::prelude::*;

use super::*;
use crate::table::testing::{Column, Keys};

/// The keys of the sequences: few, so that rows meet keys placed before.
const KEYS: i32 = 24;

#[derive(Clone, Debug)]
enum Call {
    Insert(Vec<i32>),
    FindOrInsert(Vec<i32>),
    Regrow,
    Reset,
}

fn calls() -> impl Strategy<Value = Vec<Call>> {
    let keys = || proptest::collection::vec(0..KEYS, 0..=70);
    // Fewer calls under Miri, which would take minutes.
    let most = if cfg!(miri) { 4 } else { 12 };
    proptest::collection::vec(
        prop_oneof![
            3 => keys().prop_map(Call::Insert),
            3 => keys().prop_map(Call::FindOrInsert),
            1 => Just(Call::Regrow),
            1 => Just(Call::Reset),
        ],
        1..=most,
    )
}

/// The table's records against the model: their count, a walk over them,
/// and every key's records found by a probe and the next matches.
fn check(table: &mut OwnedTable, model: &BTreeMap<i32, usize>) -> Result<()> {
    let total: usize = model.values().sum();
    ensure!(table.table()?.stats().records == total as u64, "the count");
    ensure!(scan_all(table, 16)?.len() == total, "the walk");
    let shared = table.table()?;
    for key in 0..KEYS {
        let wanted = model.get(&key).copied().unwrap_or(0);
        let (mut rows, mut refs) = ([1], [0]);
        let mut matches = Matches::new(&mut rows, &mut refs)?;
        let probe_keys = [Column::new(&[key], None)];
        let mut left = shared.probe(&[hash_i32(key)], &Keys(&probe_keys), &mut matches)?;
        let mut found = 0;
        while left > 0 {
            found += 1;
            left = shared.next_match(&mut matches)?;
        }
        ensure!(found == wanted, "key {key}: {found} records, not {wanted}");
    }
    Ok(())
}

/// Insertions, finds or insertions, larger indexes and resets in any
/// order keep the table whole: each record linked once, each key found
/// where it was placed.
#[test]
fn the_owning_table_stays_whole_under_any_order_of_its_calls() {
    property(calls(), |calls| -> Result<()> {
        // Chunks of eight records and an index for four: the calls add
        // chunks and grow the index as they go.
        let mut table = OwnedTable::new(&ONE_INT4, 4, CHUNK_HEADER + 8 * 32)?;
        let mut model = BTreeMap::new();
        for call in &calls {
            match call {
                Call::Insert(values) | Call::FindOrInsert(values) => {
                    let keys = [Column::new(values, None)];
                    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
                    let words = all_rows(values.len());
                    let rows = RowMaskView::try_new(values.len(), &words)?;
                    let mut refs = vec![0; values.len()];
                    if let Call::Insert(_) = call {
                        table.insert(&hashes, &Keys(&keys), rows, None, &mut refs)?;
                        for &value in values {
                            *model.entry(value).or_insert(0) += 1;
                        }
                    } else {
                        let mut inserted = vec![0; words.len()];
                        table.find_or_insert(
                            &hashes,
                            &Keys(&keys),
                            rows,
                            &mut refs,
                            &mut RowMask::try_new(values.len(), &mut inserted)?,
                        )?;
                        // A key the table holds keeps its records; a new
                        // one gets one.
                        for &value in values {
                            model.entry(value).or_insert(1);
                        }
                    }
                    let shared = table.table()?;
                    for (row, &value) in values.iter().enumerate() {
                        let record = shared.record(refs[row])?;
                        ensure!(record.keys == [i64::from(value)], "row {row}");
                    }
                }
                Call::Regrow => {
                    let buckets = table.table()?.stats().buckets;
                    table.regrow(buckets)?;
                }
                Call::Reset => {
                    table.reset()?;
                    model.clear();
                }
            }
            check(&mut table, &model).map_err(|error| error.context(format!("{call:?}")))?;
        }
        Ok(())
    });
}

/// An old reference after a reset reads what its place holds now, and new
/// rows take the place again; Miri runs this among the table's tests.
#[test]
fn an_old_reference_after_a_reset_reads_what_its_place_holds() -> Result<()> {
    let mut table = local(&ONE_INT4, 4)?;
    let one = [Column::new(&[1], None)];
    let old = insert_all(
        &mut table,
        &[hash_i32(1)],
        &Keys(&one),
        Some(&7u64.to_ne_bytes()),
    )?;
    table.reset()?;
    {
        let shared = table.table()?;
        let record = shared.record(old[0])?;
        ensure!(record.keys == [1], "the bytes stay until written");
    }
    let two = [Column::new(&[2], None)];
    let new = insert_all(
        &mut table,
        &[hash_i32(2)],
        &Keys(&two),
        Some(&9u64.to_ne_bytes()),
    )?;
    ensure!(new == old, "the place is taken again");
    let shared = table.table()?;
    let record = shared.record(old[0])?;
    ensure!(record.keys == [2] && record.payload == 9u64.to_ne_bytes());
    Ok(())
}
