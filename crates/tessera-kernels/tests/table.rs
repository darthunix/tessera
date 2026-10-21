//! The kernels over the table: keys read from columns, and the states of
//! aggregates folded into the records of groups and merged. The table's
//! own tests are the crate's.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test reports a failure by panicking"
)]
#![deny(unsafe_code)]

use anyhow::Result;
use proptest::prelude::*;
use tessera_core::{ColumnReader, ColumnView, RowMask, RowMaskView};
use tessera_kernels::accumulate::{self, ExtremeSlot, Fold, Slot, SumSlot};
use tessera_kernels::column_keys::{KeyColumns, normalize_word};
use tessera_kernels::decimal::{
    self, Decimal, DecimalWord, ExtremeState, ExtremeValue, Offer, Partial, Partials, Special,
    SumState, Term, Terms,
};
use tessera_kernels::int32::murmurhash32;
use tessera_kernels::ops::ArithmeticError;
use tessera_kernels::table::{
    CHUNK_HEADER, Cursor, KeySource, Matches, MatchesView, OwnedTable, Table, TableConfig,
};
use tessera_testing::{flags, integer, property, values, words};

/// Chunks of the tests: small, so that most tables span several.
const CHUNK: usize = 4096;

const ONE_INT4: TableConfig = TableConfig {
    nkeys: 1,
    payload_size: 8,
};

/// An empty table for `capacity` records of `config` over chunks of
/// [`CHUNK`] bytes.
fn local(config: &TableConfig, capacity: u64) -> Result<OwnedTable> {
    Ok(OwnedTable::new(config, capacity, CHUNK)?)
}

fn hash_i32(value: i32) -> u32 {
    murmurhash32(value as u32)
}

/// Some hash of an int8; the table takes whatever the caller computes.
fn hash_i64(value: i64) -> u32 {
    murmurhash32((value as u64 ^ (value as u64 >> 32)) as u32)
}

/// Mask words with every one of `nrows` rows set.
fn all_rows(nrows: usize) -> Vec<u64> {
    let mut words = vec![u64::MAX; nrows.div_ceil(64)];
    if !nrows.is_multiple_of(64) {
        *words.last_mut().unwrap() = (1 << (nrows % 64)) - 1;
    }
    words
}

/// The rows a mask holds.
fn rows_of(view: &RowMaskView<'_>) -> Vec<usize> {
    view.selected_rows().collect()
}

/// Append and link every row of a batch, expecting all of them to go in.
fn insert_all<K: KeySource + ?Sized>(
    table: &mut OwnedTable,
    hashes: &[u32],
    keys: &K,
    payload: Option<&[u8]>,
) -> Result<Vec<u32>> {
    let nrows = hashes.len();
    let rows = all_rows(nrows);
    let mut offsets = vec![0; nrows];
    let inserted = table.insert(
        hashes,
        keys,
        RowMaskView::try_new(nrows, &rows)?,
        payload,
        &mut offsets,
    )?;
    assert_eq!(inserted, nrows);
    Ok(offsets)
}

/// Probe every row of a batch: the rows found and their matches.
fn probe_all<K: KeySource + ?Sized>(
    table: &Table<'_>,
    hashes: &[u32],
    keys: &K,
) -> Result<(Vec<usize>, Vec<u32>)> {
    let nrows = hashes.len();
    let mut rows = all_rows(nrows);
    let mut refs = vec![0; nrows];
    let mut matches = Matches::new(&mut rows, &mut refs)?;
    let left = table.probe(hashes, keys, &mut matches)?;
    let found = rows_of(&matches.rows());
    assert_eq!(left, found.len(), "the call counts the rows it keeps");
    Ok((found, refs))
}

/// Resolve every row of a batch to a record, adding chunks and building a
/// larger index as needed: the references and the rows whose record was
/// created.
fn resolve_all<K: KeySource + ?Sized>(
    table: &mut OwnedTable,
    hashes: &[u32],
    keys: &K,
) -> Result<(Vec<u32>, Vec<usize>)> {
    let nrows = hashes.len();
    let rows = all_rows(nrows);
    let mut offsets = vec![0; nrows];
    let mut inserted_words = vec![0; nrows.div_ceil(64)];
    let mut inserted = RowMask::try_new(nrows, &mut inserted_words)?;
    let resolved = table.find_or_insert(
        hashes,
        keys,
        RowMaskView::try_new(nrows, &rows)?,
        &mut offsets,
        &mut inserted,
    )?;
    assert_eq!(resolved, nrows);
    let created = rows_of(&inserted.as_view());
    Ok((offsets, created))
}

/// Every record of a table in the order it was appended, through walks of
/// `step`.
fn scan_all(table: &mut OwnedTable, step: usize) -> Result<Vec<u32>> {
    let table = table.table_mut()?;
    let mut cursor = Cursor::start();
    let mut out = vec![0; step];
    let mut all = Vec::new();
    loop {
        let count = table.scan(&mut cursor, &mut out)?;
        if count == 0 {
            break;
        }
        all.extend_from_slice(&out[..count]);
    }
    assert_eq!(table.scan(&mut cursor, &mut out)?, 0, "the walk stays over");
    Ok(all)
}

/// A column that hides its storage, so that every word goes row by row.
struct RowsOnly<C>(C);

impl<C: ColumnReader> ColumnReader for RowsOnly<C> {
    type Value = C::Value;

    fn nrows(&self) -> usize {
        self.0.nrows()
    }

    fn get(&self, row: usize) -> Result<Option<C::Value>> {
        self.0.get(row)
    }

    fn word_values(
        &self,
        word_index: usize,
        selected: u64,
    ) -> Result<impl Iterator<Item = (usize, Option<C::Value>)> + '_> {
        self.0.word_values(word_index, selected)
    }
}

/// A key column, its non-NULL flags (values under NULL are edges) and a
/// selection to normalize besides the fixed ones.
fn key_columns() -> impl Strategy<Value = (Vec<i32>, Vec<bool>, Vec<bool>)> {
    (1..=200_usize)
        .prop_flat_map(|nrows| (flags(nrows), flags(nrows)))
        .prop_flat_map(|(non_null, selected)| {
            (
                values(&non_null, &integer::<i32>()),
                Just(non_null),
                Just(selected),
            )
        })
}

#[test]
fn whole_words_normalize_like_rows() {
    property(key_columns(), |(values, non_null, selected)| {
        whole_words_normalize_like(&values, &non_null, &selected)
    });
}

fn whole_words_normalize_like(values: &[i32], non_null: &[bool], selected: &[bool]) -> Result<()> {
    let nrows = values.len();
    let non_nulls = words(non_null);
    let drawn = words(selected);
    {
        let dense = ColumnView::try_new(values, Some(RowMaskView::try_new(nrows, &non_nulls)?))?;
        let rows = RowsOnly(ColumnView::try_new(
            values,
            Some(RowMaskView::try_new(nrows, &non_nulls)?),
        )?);
        for (index, &drawn) in drawn.iter().enumerate() {
            let width = (nrows - index * 64).min(64);
            let full = if width == 64 {
                u64::MAX
            } else {
                (1 << width) - 1
            };
            for selected in [
                full,
                full & 0x5555_5555_5555_5555,
                full & 0x8000_0000_0000_0001,
                drawn,
            ] {
                let (mut from_block, mut from_rows) = ([7; 64], [7; 64]);
                let block_bits = normalize_word(&dense, index, selected, &mut from_block).unwrap();
                let row_bits = normalize_word(&rows, index, selected, &mut from_rows).unwrap();
                assert_eq!(block_bits, row_bits, "{nrows} rows, word {index}");
                for bit in (0..64).filter(|bit| selected >> bit & 1 != 0) {
                    assert_eq!(from_block[bit], from_rows[bit], "{nrows} rows, row {bit}");
                    if row_bits >> bit & 1 == 0 {
                        assert_eq!(from_block[bit], 0, "a NULL key is 0");
                    }
                }
            }
        }
        // Both paths build tables that answer alike: the same record at
        // each reference, and the same rows found with the same matches.
        let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
        let (dense, rows) = ([dense], [rows]);
        let mut first = local(&ONE_INT4, nrows as u64)?;
        let mut second = local(&ONE_INT4, nrows as u64)?;
        let offsets = insert_all(&mut first, &hashes, &KeyColumns(&dense), None)?;
        let other = insert_all(&mut second, &hashes, &KeyColumns(&rows), None)?;
        assert_eq!(offsets, other);
        let (first, second) = (first.table()?, second.table()?);
        for &offset in &offsets {
            let (one, other) = (first.record(offset)?, second.record(offset)?);
            assert_eq!(
                (one.hash, one.null_bits, one.keys),
                (other.hash, other.null_bits, other.keys)
            );
        }
        assert_eq!(
            probe_all(&first, &hashes, &KeyColumns(&dense))?,
            probe_all(&second, &hashes, &KeyColumns(&rows))?
        );
    }
    Ok(())
}

/// The state of one group in the model of a grouped aggregate.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct GroupModel {
    rows: i64,
    values: i64,
    sum: Option<i64>,
    min4: Option<i64>,
    max8: Option<i64>,
}

#[test]
fn grouped_states_follow_a_row_by_row_model() -> Result<()> {
    // Payload: flags, count(*), count(x), sum(x), min(x) of int4, max(y) of int8.
    const FLAGS_AT: usize = 0;
    let config = TableConfig {
        nkeys: 1,
        payload_size: 48,
    };
    let nrows = 200;
    let keys: Vec<i32> = (0..nrows).map(|row| (row as i32 * 7) % 13 - 6).collect();
    let key_present: Vec<bool> = (0..nrows).map(|row| row % 11 != 4).collect();
    let xs: Vec<i32> = (0..nrows).map(|row| (row as i32 * 31) % 97 - 48).collect();
    let x_present: Vec<bool> = (0..nrows).map(|row| row % 5 != 2).collect();
    let ys: Vec<i64> = (0..nrows).map(|row| (row as i64 - 100) << 33).collect();
    let words_of = |flags: &[bool]| -> Vec<u64> {
        let mut words = vec![0; flags.len().div_ceil(64)];
        for (row, &flag) in flags.iter().enumerate() {
            words[row / 64] |= u64::from(flag) << (row % 64);
        }
        words
    };
    let key_words = words_of(&key_present);
    let x_words = words_of(&x_present);
    let key_column = [ColumnView::try_new(
        &keys,
        Some(RowMaskView::try_new(nrows, &key_words)?),
    )?];
    let x_column = ColumnView::try_new(&xs, Some(RowMaskView::try_new(nrows, &x_words)?))?;
    let y_column = ColumnView::try_new(&ys, None)?;
    let hashes: Vec<u32> = (0..nrows)
        .map(|row| {
            if key_present[row] {
                hash_i32(keys[row])
            } else {
                0x9e37_79b9
            }
        })
        .collect();
    let mut table = OwnedTable::new(&config, 64, CHUNK_HEADER + 5 * 64)?;
    let (offsets, _) = resolve_all(&mut table, &hashes, &KeyColumns(&key_column))?;
    assert!(table.chunks() > 1, "the groups span chunks");
    // Two batches over the same rows: the first and the second half.
    for half in 0..2 {
        let selected: Vec<bool> = (0..nrows)
            .map(|row| (row < nrows / 2) == (half == 0))
            .collect();
        let selection = words_of(&selected);
        let rows = RowMaskView::try_new(nrows, &selection)?;
        let mut writer = table.table_mut()?;
        accumulate::count_rows(&mut writer, &offsets, &rows, 8)?;
        accumulate::count_values(&mut writer, &offsets, &rows, &x_column, 16)?;
        let slot = |value_at, flag_bit| Slot {
            value_at,
            flags_at: FLAGS_AT,
            flag_bit,
        };
        accumulate::fold(
            &mut writer,
            &offsets,
            &rows,
            &x_column,
            Fold::Sum,
            slot(24, 0),
        )?;
        accumulate::fold(
            &mut writer,
            &offsets,
            &rows,
            &x_column,
            Fold::Min,
            slot(32, 1),
        )?;
        accumulate::fold(
            &mut writer,
            &offsets,
            &rows,
            &y_column,
            Fold::Max,
            slot(40, 2),
        )?;
    }
    let mut model: std::collections::HashMap<Option<i32>, GroupModel> = Default::default();
    for row in 0..nrows {
        let group = model
            .entry(key_present[row].then_some(keys[row]))
            .or_default();
        group.rows += 1;
        if x_present[row] {
            let x = i64::from(xs[row]);
            group.values += 1;
            group.sum = Some(group.sum.unwrap_or(0) + x);
            group.min4 = Some(group.min4.map_or(x, |m| m.min(x)));
        }
        group.max8 = Some(group.max8.map_or(ys[row], |m| m.max(ys[row])));
    }
    let groups = scan_all(&mut table, 7)?;
    assert_eq!(groups.len(), model.len());
    let table = table.table()?;
    let all = all_rows(groups.len());
    let rows = RowMaskView::try_new(groups.len(), &all)?;
    let mut key_values = vec![0; groups.len()];
    let mut key_nulls = vec![false; groups.len()];
    table.gather_key(
        MatchesView::new(rows, &groups)?,
        0,
        &mut key_values,
        &mut key_nulls,
    )?;
    let mut fields = [(); 6].map(|_| vec![0u64; groups.len()]);
    for (index, field) in fields.iter_mut().enumerate() {
        table.gather(MatchesView::new(rows, &groups)?, index * 8, field)?;
    }
    for group in 0..groups.len() {
        let key = (!key_nulls[group]).then_some(key_values[group] as i64 as i32);
        assert_eq!(
            key_values[group] as i64,
            key.map_or(0, i64::from),
            "the int4 key is sign-extended"
        );
        let flags = fields[0][group];
        let state =
            |bit: u32, field: usize| (flags >> bit & 1 == 1).then_some(fields[field][group] as i64);
        let found = GroupModel {
            rows: fields[1][group] as i64,
            values: fields[2][group] as i64,
            sum: state(0, 3),
            min4: state(1, 4),
            max8: state(2, 5),
        };
        assert_eq!(Some(&found), model.get(&key), "group {key:?}");
    }
    Ok(())
}

/// Terms of a sum by row, as a batch source gives them: some only term by
/// term, some also in bulk, the decimals of one scale.
struct TermColumn {
    terms: Vec<Term>,
    bulk: Option<u32>,
}

impl Terms for TermColumn {
    fn term(&self, row: usize) -> Term {
        self.terms[row]
    }

    fn fold_decimals(
        &self,
        index: usize,
        rows: u64,
        mut add: impl FnMut(usize, i64),
    ) -> Option<DecimalWord> {
        let scale = self.bulk?;
        let mut bulk = 0;
        for bit in (0..64).filter(|bit| rows >> bit & 1 == 1) {
            if let Term::Decimal(decimal) = self.terms[index * 64 + bit]
                && decimal.scale() == scale
            {
                add(bit, decimal.value());
                bulk |= 1 << bit;
            }
        }
        Some(DecimalWord { rows: bulk, scale })
    }
}

/// The terms of one sum of a model: row `row` of `column`.
fn model_term(column: usize, row: usize) -> Term {
    match (row + 3 * column) % 29 {
        0 => Term::Null,
        1 => Term::Other,
        2 => Term::Special(Special::NaN),
        3 if row.is_multiple_of(2) => Term::Special(Special::PositiveInfinity),
        3 => Term::Special(Special::NegativeInfinity),
        // A scale past 18 the sum refuses.
        4 => Term::Decimal(Decimal::new(5, 20).unwrap()),
        // Values near 10^18 at scale 0, which pass the bound at scale 18.
        5 => Term::Decimal(Decimal::new(999_999_999_999_999_999, 0).unwrap()),
        // Another scale within a word's group now and then.
        6 | 7 => Term::Decimal(Decimal::new(row as i64 - 150, 3).unwrap()),
        _ => Term::Decimal(
            Decimal::new(
                (row as i64 * 7919 + column as i64) % 20001 - 10000,
                u32::from(column == 1) * 2,
            )
            .unwrap(),
        ),
    }
}

/// Sums and averages kept in records, three to a call: every group's
/// states are the ones a row-by-row model adds up, and the rows a state
/// refuses go to its rest, whether a word's groups fit the local sums (few
/// keys) or not (many), a group's state starting at the bound or not.
#[test]
fn sum_states_follow_a_row_by_row_model() -> Result<()> {
    const SUMS: usize = 3;
    for (distinct, near_bound) in [(5, false), (11, true), (200, false), (200, true)] {
        let config = TableConfig {
            nkeys: 1,
            payload_size: 8 + 8 * SumState::WORDS * SUMS,
        };
        let nrows: usize = 300;
        let keys: Vec<i32> = (0..nrows)
            .map(|row| ((row * 7) % distinct) as i32)
            .collect();
        let key_column = [ColumnView::try_new(&keys, None)?];
        let hashes: Vec<u32> = keys.iter().map(|&key| hash_i32(key)).collect();
        // Sum 0 term by term; sums 1 and 2 in bulk at scales 2 and 0, their
        // other terms one by one.
        let terms = |column: usize| (0..nrows).map(|row| model_term(column, row)).collect();
        let columns = [
            TermColumn {
                terms: terms(0),
                bulk: None,
            },
            TermColumn {
                terms: terms(1),
                bulk: Some(2),
            },
            TermColumn {
                terms: terms(2),
                bulk: Some(0),
            },
        ];
        let mut table = OwnedTable::new(&config, 256, CHUNK_HEADER + 64 * 128)?;
        let (offsets, _) = resolve_all(&mut table, &hashes, &KeyColumns(&key_column))?;
        let at = |sum: usize| 8 + 8 * SumState::WORDS * sum;
        let mut model: std::collections::HashMap<i32, [SumState; SUMS]> = Default::default();
        if near_bound {
            // Within 10^20 of the bound at scale 3, the largest of the terms.
            let first = SumState {
                sum: decimal::Sum {
                    value: decimal::SUM_BOUND - 100_000_000_000_000_000_000,
                    scale: 3,
                    count: 1,
                },
                ..SumState::default()
            };
            // Every group starts there.
            let mut writer = table.table_mut()?;
            for (row, &offset) in offsets.iter().enumerate() {
                if model.contains_key(&keys[row]) {
                    continue;
                }
                let payload = writer.payload_mut(offset)?;
                for sum in 0..SUMS {
                    for (word, value) in first.to_words().into_iter().enumerate() {
                        payload[at(sum) + 8 * word..at(sum) + 8 * word + 8]
                            .copy_from_slice(&value.to_ne_bytes());
                    }
                }
                model.insert(keys[row], [first; SUMS]);
            }
        }
        let mut refused = [false; SUMS];
        // Two batches over the same rows, the second over odd rows only.
        for pass in 0..2 {
            let mut selection: Vec<u64> = vec![
                if pass == 0 {
                    u64::MAX
                } else {
                    0xAAAA_AAAA_AAAA_AAAA
                };
                nrows.div_ceil(64)
            ];
            *selection.last_mut().unwrap() &= (1 << (nrows % 64)) - 1;
            let rows = RowMaskView::try_new(nrows, &selection)?;
            let mut rest_words = vec![vec![0_u64; nrows.div_ceil(64)]; SUMS];
            {
                let mut slots: Vec<SumSlot<'_, TermColumn>> = rest_words
                    .iter_mut()
                    .zip(&columns)
                    .enumerate()
                    .map(|(sum, (words, terms))| {
                        Ok(SumSlot {
                            terms,
                            at: at(sum),
                            rest: RowMask::try_new(nrows, words)?,
                        })
                    })
                    .collect::<Result<_>>()?;
                accumulate::sum_terms(&mut table.table_mut()?, &offsets, &rows, &mut slots)?;
            }
            let mut expected = vec![vec![0_u64; nrows.div_ceil(64)]; SUMS];
            for row in rows_of(&rows) {
                let states = model.entry(keys[row]).or_default();
                for (sum, state) in states.iter_mut().enumerate() {
                    if !state.add(columns[sum].terms[row]) {
                        expected[sum][row / 64] |= 1 << (row % 64);
                    }
                }
            }
            for sum in 0..SUMS {
                assert_eq!(
                    rest_words[sum], expected[sum],
                    "rest of sum {sum}, pass {pass}, {distinct} keys"
                );
                refused[sum] |= (0..nrows).any(|row| {
                    expected[sum][row / 64] >> (row % 64) & 1 == 1
                        && columns[sum].terms[row] != Term::Other
                        && !matches!(columns[sum].terms[row], Term::Decimal(decimal) if decimal.scale() > 18)
                });
            }
        }
        assert_eq!(
            refused.iter().any(|&sum| sum),
            near_bound,
            "a sum at its bound"
        );
        let groups = scan_all(&mut table, 5)?;
        assert_eq!(groups.len(), model.len());
        let table = table.table()?;
        let all = all_rows(groups.len());
        let rows = RowMaskView::try_new(groups.len(), &all)?;
        let mut key_values = vec![0; groups.len()];
        let mut key_nulls = vec![false; groups.len()];
        table.gather_key(
            MatchesView::new(rows, &groups)?,
            0,
            &mut key_values,
            &mut key_nulls,
        )?;
        for sum in 0..SUMS {
            let mut fields = [(); SumState::WORDS].map(|_| vec![0u64; groups.len()]);
            for (index, field) in fields.iter_mut().enumerate() {
                table.gather(MatchesView::new(rows, &groups)?, at(sum) + index * 8, field)?;
            }
            for group in 0..groups.len() {
                let found = SumState::from_words(std::array::from_fn(|word| fields[word][group]));
                let key = key_values[group] as i64 as i32;
                assert_eq!(
                    Some(&found),
                    model.get(&key).map(|states| &states[sum]),
                    "group {key}, sum {sum}, {distinct} keys"
                );
            }
        }
    }
    Ok(())
}

/// `min` and `max` of numeric kept in records: every group's state is the
/// one a row-by-row model offers the rows to, in their order, and the rows
/// a state leaves go to the rest, a pending group's later rows after them,
/// whether a group starts with a longer value or not. The caller takes the
/// rows left and clears pending after each batch.
#[test]
fn extreme_states_follow_a_row_by_row_model() -> Result<()> {
    for (distinct, max, numeric) in [(5, true, false), (11, false, true), (200, true, true)] {
        let config = TableConfig {
            nkeys: 1,
            payload_size: 8 + 8 * ExtremeState::WORDS,
        };
        let nrows: usize = 300;
        let keys: Vec<i32> = (0..nrows)
            .map(|row| ((row * 7) % distinct) as i32)
            .collect();
        let key_column = [ColumnView::try_new(&keys, None)?];
        let hashes: Vec<u32> = keys.iter().map(|&key| hash_i32(key)).collect();
        let column = TermColumn {
            terms: (0..nrows).map(|row| model_term(1, row)).collect(),
            bulk: None,
        };
        let mut table = OwnedTable::new(&config, 256, CHUNK_HEADER + 64 * 128)?;
        let (offsets, _) = resolve_all(&mut table, &hashes, &KeyColumns(&key_column))?;
        let at = 8;
        let mut model: std::collections::HashMap<i32, ExtremeState> = Default::default();
        let write = |table: &mut OwnedTable, offset: u32, state: ExtremeState| -> Result<()> {
            let (value, flags) = state.to_words();
            let mut writer = table.table_mut()?;
            let payload = writer.payload_mut(offset)?;
            payload[at..at + 8].copy_from_slice(&value.to_ne_bytes());
            payload[at + 8..at + 16].copy_from_slice(&flags.to_ne_bytes());
            Ok(())
        };
        for (row, &offset) in offsets.iter().enumerate() {
            let first = if numeric && keys[row] % 2 == 0 {
                // A longer value the caller keeps: decimals wait for it.
                ExtremeState {
                    value: ExtremeValue::Numeric,
                    pending: false,
                }
            } else {
                ExtremeState::EMPTY
            };
            if model.insert(keys[row], first).is_none() {
                write(&mut table, offset, first)?;
            }
        }
        let mut left = 0;
        // Two batches over the same rows, the second over odd rows only.
        for pass in 0..2 {
            let mut selection: Vec<u64> = vec![
                if pass == 0 {
                    u64::MAX
                } else {
                    0xAAAA_AAAA_AAAA_AAAA
                };
                nrows.div_ceil(64)
            ];
            *selection.last_mut().unwrap() &= (1 << (nrows % 64)) - 1;
            let rows = RowMaskView::try_new(nrows, &selection)?;
            let mut rest = vec![0_u64; nrows.div_ceil(64)];
            accumulate::extremes(
                &mut table.table_mut()?,
                &offsets,
                &rows,
                &mut ExtremeSlot {
                    terms: &column,
                    at,
                    max,
                    rest: RowMask::try_new(nrows, &mut rest)?,
                },
            )?;
            let mut expected = vec![0_u64; nrows.div_ceil(64)];
            for row in rows_of(&rows) {
                let state = model.get_mut(&keys[row]).unwrap();
                if state.offer(column.terms[row], max) == Offer::Rest {
                    expected[row / 64] |= 1 << (row % 64);
                    left += 1;
                }
            }
            assert_eq!(rest, expected, "rest, pass {pass}, {distinct} keys");
            // The caller clears pending once it has taken the rows.
            for (row, &offset) in offsets.iter().enumerate() {
                let state = model.get_mut(&keys[row]).unwrap();
                state.pending = false;
                write(&mut table, offset, *state)?;
            }
        }
        assert!(left > 0, "rows left to the caller");
        let groups = scan_all(&mut table, 5)?;
        assert_eq!(groups.len(), model.len());
        let table = table.table()?;
        let all = all_rows(groups.len());
        let rows = RowMaskView::try_new(groups.len(), &all)?;
        let mut key_values = vec![0; groups.len()];
        let mut key_nulls = vec![false; groups.len()];
        table.gather_key(
            MatchesView::new(rows, &groups)?,
            0,
            &mut key_values,
            &mut key_nulls,
        )?;
        let mut fields = [(); 2].map(|_| vec![0u64; groups.len()]);
        for (index, field) in fields.iter_mut().enumerate() {
            table.gather(MatchesView::new(rows, &groups)?, at + index * 8, field)?;
        }
        let mut taken = 0;
        for group in 0..groups.len() {
            let found = ExtremeState::from_words(fields[0][group], fields[1][group])?;
            let key = key_values[group] as i64 as i32;
            assert_eq!(
                Some(&found),
                model.get(&key),
                "group {key}, {distinct} keys"
            );
            taken += usize::from(matches!(found.value, ExtremeValue::Decimal(_)));
        }
        assert!(taken > 0, "groups with a decimal extreme");
    }
    Ok(())
}

/// Partial states of sums by row, as a final grouping's batch gives them.
struct PartialColumn(Vec<Partial>);

impl Partials for PartialColumn {
    fn partial(&self, row: usize) -> Result<Partial> {
        Ok(self.0[row])
    }
}

/// Partial states of three participants, each folding its share of the
/// rows into states of its own, merged into one table: a record's state is
/// the merge of its partial states in row order, NULL skipped, a state with
/// a rest or one past the record's bound left in the rest; and where no
/// state went to a rest, the merged state is the one of all the rows at
/// once, as exact sums are whatever the split.
#[test]
fn partial_states_merge_as_the_rows_would() -> Result<()> {
    const SUMS: usize = 3;
    const PARTICIPANTS: usize = 3;
    for (distinct, near_bound) in [(5, false), (11, true), (200, false)] {
        let config = TableConfig {
            nkeys: 1,
            payload_size: 8 + 8 * SumState::WORDS * SUMS,
        };
        let nrows: usize = 600;
        let key = |row: usize| ((row * 7) % distinct) as i32;
        // Each participant's states by key, and whether a term went to
        // its rest; the states of all rows at once, the same way.
        let mut partials: Vec<std::collections::BTreeMap<i32, [(SumState, bool); SUMS]>> =
            vec![Default::default(); PARTICIPANTS];
        let mut whole: std::collections::HashMap<i32, [(SumState, bool); SUMS]> =
            Default::default();
        // Sum 0 has the model's terms, rests among them; sums 1 and 2 only
        // decimals of scales 0 to 2, NULL and NaN or -Infinity, which every
        // state takes.
        let term = |sum: usize, row: usize| match (sum, (row + sum) % 17) {
            (0, _) => model_term(0, row),
            (_, 0) => Term::Null,
            (1, 1) => Term::Special(Special::NaN),
            (2, 1) if row.is_multiple_of(3) => Term::Special(Special::NegativeInfinity),
            _ => Term::Decimal(
                Decimal::new(
                    (row as i64 * 7919 + sum as i64) % 20001 - 10000,
                    (row % 3) as u32,
                )
                .unwrap(),
            ),
        };
        for row in 0..nrows {
            let states = partials[row % PARTICIPANTS].entry(key(row)).or_default();
            let all = whole.entry(key(row)).or_default();
            for sum in 0..SUMS {
                let term = term(sum, row);
                if !states[sum].0.add(term) {
                    states[sum].1 = true;
                }
                if !all[sum].0.add(term) {
                    all[sum].1 = true;
                }
            }
        }
        // The final grouping's rows: a participant's groups in turn, an
        // empty state as NULL.
        let mut keys = Vec::new();
        let mut columns: [Vec<Partial>; SUMS] = Default::default();
        for participant in &partials {
            for (&key, states) in participant {
                keys.push(key);
                for (column, &(state, rest)) in columns.iter_mut().zip(states) {
                    column.push(if rest {
                        Partial::Other
                    } else if state == SumState::default() {
                        Partial::Null
                    } else {
                        Partial::State(state)
                    });
                }
            }
        }
        let nfinal = keys.len();
        let key_column = [ColumnView::try_new(&keys, None)?];
        let hashes: Vec<u32> = keys.iter().map(|&key| hash_i32(key)).collect();
        let mut table = OwnedTable::new(&config, 512, CHUNK_HEADER + 64 * 128)?;
        let (offsets, _) = resolve_all(&mut table, &hashes, &KeyColumns(&key_column))?;
        let at = |sum: usize| 8 + 8 * SumState::WORDS * sum;
        let mut model: std::collections::HashMap<i32, [SumState; SUMS]> = Default::default();
        if near_bound {
            // Every group starts within 10^20 of the bound at scale 0: a
            // partial sum of a larger scale takes it past the bound.
            let first = SumState {
                sum: decimal::Sum {
                    value: decimal::SUM_BOUND - 100_000_000_000_000_000_000,
                    scale: 0,
                    count: 1,
                },
                ..SumState::default()
            };
            let mut writer = table.table_mut()?;
            for (row, &offset) in offsets.iter().enumerate() {
                if model.contains_key(&keys[row]) {
                    continue;
                }
                let payload = writer.payload_mut(offset)?;
                for sum in 0..SUMS {
                    for (word, value) in first.to_words().into_iter().enumerate() {
                        payload[at(sum) + 8 * word..at(sum) + 8 * word + 8]
                            .copy_from_slice(&value.to_ne_bytes());
                    }
                }
                model.insert(keys[row], [first; SUMS]);
            }
        }
        let columns = columns.map(PartialColumn);
        let selection = all_rows(nfinal);
        let rows = RowMaskView::try_new(nfinal, &selection)?;
        let mut rest_words = vec![vec![0_u64; nfinal.div_ceil(64)]; SUMS];
        {
            let mut slots: Vec<SumSlot<'_, PartialColumn>> = rest_words
                .iter_mut()
                .zip(&columns)
                .enumerate()
                .map(|(sum, (words, terms))| {
                    Ok(SumSlot {
                        terms,
                        at: at(sum),
                        rest: RowMask::try_new(nfinal, words)?,
                    })
                })
                .collect::<Result<_>>()?;
            accumulate::sum_partials(&mut table.table_mut()?, &offsets, &rows, &mut slots)?;
        }
        let mut expected = vec![vec![0_u64; nfinal.div_ceil(64)]; SUMS];
        let mut refused = false;
        for row in 0..nfinal {
            let states = model.entry(keys[row]).or_default();
            for (sum, state) in states.iter_mut().enumerate() {
                let taken = match columns[sum].0[row] {
                    Partial::Null => true,
                    Partial::State(partial) => {
                        let merged = state.merge(partial);
                        refused |= !merged;
                        merged
                    }
                    Partial::Other => false,
                };
                if !taken {
                    expected[sum][row / 64] |= 1 << (row % 64);
                }
            }
        }
        assert_eq!(refused, near_bound, "a merge past the bound");
        for sum in 0..SUMS {
            assert_eq!(
                rest_words[sum], expected[sum],
                "rest of sum {sum}, {distinct} keys"
            );
        }
        let groups = scan_all(&mut table, 7)?;
        assert_eq!(groups.len(), model.len());
        let table = table.table()?;
        let all = all_rows(groups.len());
        let rows = RowMaskView::try_new(groups.len(), &all)?;
        let mut key_values = vec![0; groups.len()];
        let mut key_nulls = vec![false; groups.len()];
        table.gather_key(
            MatchesView::new(rows, &groups)?,
            0,
            &mut key_values,
            &mut key_nulls,
        )?;
        let mut matched_whole = 0;
        for sum in 0..SUMS {
            let mut fields = [(); SumState::WORDS].map(|_| vec![0u64; groups.len()]);
            for (index, field) in fields.iter_mut().enumerate() {
                table.gather(MatchesView::new(rows, &groups)?, at(sum) + index * 8, field)?;
            }
            for group in 0..groups.len() {
                let found = SumState::from_words(std::array::from_fn(|word| fields[word][group]));
                let key = key_values[group] as i64 as i32;
                assert_eq!(
                    Some(&found),
                    model.get(&key).map(|states| &states[sum]),
                    "group {key}, sum {sum}, {distinct} keys"
                );
                // Without a rest anywhere, the merge is the whole fold.
                let (all, rest) = whole[&key][sum];
                let any_rest = partials
                    .iter()
                    .any(|participant| participant.get(&key).is_some_and(|states| states[sum].1));
                if !near_bound && !rest && !any_rest {
                    assert_eq!(
                        found, all,
                        "group {key}, sum {sum}: the merge is the whole fold"
                    );
                    matched_whole += 1;
                }
            }
        }
        assert!(
            near_bound || matched_whole > 0,
            "some groups merged without a rest"
        );
    }
    Ok(())
}

/// A merge adds the sums at the larger scale and keeps the flags; one past
/// the bound leaves the state as it was.
#[test]
fn a_state_merges_another() {
    let state = |value: i128, scale: u32, count: u64| SumState {
        sum: decimal::Sum {
            value,
            scale,
            count,
        },
        ..SumState::default()
    };
    let mut ours = state(125, 2, 3);
    assert!(ours.merge(SumState {
        nan: true,
        ..state(-7, 3, 2)
    }));
    assert_eq!(
        ours,
        SumState {
            nan: true,
            ..state(1243, 3, 5)
        }
    );
    // An empty state changes nothing; one of flags alone adds its flags.
    assert!(ours.merge(SumState::default()));
    assert!(ours.merge(SumState {
        negative_infinity: true,
        ..SumState::default()
    }));
    assert_eq!(
        ours,
        SumState {
            nan: true,
            negative_infinity: true,
            ..state(1243, 3, 5)
        }
    );
    // Past the bound: refused, unchanged.
    let before = ours;
    assert!(!ours.merge(SumState {
        positive_infinity: true,
        ..state(decimal::SUM_BOUND - 1, 3, 1)
    }));
    assert_eq!(ours, before);
}

#[test]
fn a_sum_past_the_bigint_range_fails_and_arguments_are_checked() -> Result<()> {
    let config = TableConfig {
        nkeys: 1,
        payload_size: 16,
    };
    let keys = [ColumnView::try_new(&[-5i64, -5, 1 << 40], None)?];
    let hashes: Vec<u32> = [-5i64, -5, 1 << 40].map(hash_i64).into();
    let mut table = local(&config, 8)?;
    let (offsets, created) = resolve_all(&mut table, &hashes, &KeyColumns(&keys))?;
    assert_eq!(created, vec![0, 2]);
    let all = all_rows(3);
    let rows = RowMaskView::try_new(3, &all)?;
    let slot = Slot {
        value_at: 8,
        flags_at: 0,
        flag_bit: 0,
    };
    let big = ColumnView::try_new(&[i64::MAX, 1, 0], None)?;
    let mut writer = table.table_mut()?;
    let error = accumulate::fold(&mut writer, &offsets, &rows, &big, Fold::Sum, slot).unwrap_err();
    assert_eq!(
        error.downcast_ref::<ArithmeticError>().copied(),
        Some(ArithmeticError::BigintOutOfRange)
    );
    // The int8 keys come back whole, a negative one too.
    let mut values = vec![0; 3];
    let mut nulls = vec![true; 3];
    writer.gather_key(
        MatchesView::new(rows, &offsets)?,
        0,
        &mut values,
        &mut nulls,
    )?;
    assert_eq!(values, [(-5i64) as u64, (-5i64) as u64, 1 << 40]);
    assert_eq!(nulls, [false; 3]);
    // Words past the payload, a flag past a word, a key past the keys.
    assert!(accumulate::count_rows(&mut writer, &offsets, &rows, 16).is_err());
    assert!(accumulate::count_rows(&mut writer, &offsets, &rows, 3).is_err());
    let past = Slot {
        flag_bit: 64,
        ..slot
    };
    assert!(accumulate::fold(&mut writer, &offsets, &rows, &big, Fold::Min, past).is_err());
    assert!(
        writer
            .gather_key(
                MatchesView::new(rows, &offsets)?,
                1,
                &mut values,
                &mut nulls
            )
            .is_err()
    );
    assert!(accumulate::count_rows(&mut writer, &offsets[..2], &rows, 8).is_err());
    Ok(())
}
