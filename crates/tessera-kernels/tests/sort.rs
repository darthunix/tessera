#![forbid(unsafe_code)]

use std::cmp::Ordering;

use anyhow::Result;
use tessera_core::RowMask;
use tessera_kernels::sort::{MAX_ITEM_WORDS, SortKey, item_words, sort_items};
use tessera_kernels::table::{KeyKind, KeySource, LocalTable, TableConfig};

/// Chunks of the tests: small, so that most row sets span several.
const CHUNK: usize = 4096;

/// Rows of keys, each one a value or NULL, as a key source.
struct Rows {
    kinds: Vec<KeyKind>,
    /// Per key, per row.
    values: Vec<Vec<Option<i64>>>,
}

impl KeySource for Rows {
    fn nkeys(&self) -> usize {
        self.kinds.len()
    }

    fn nrows(&self) -> usize {
        self.values[0].len()
    }

    fn word(&self, key: usize, index: usize, selected: u64, out: &mut [i64; 64]) -> Result<u64> {
        let mut non_null = 0;
        for (bit, slot) in out.iter_mut().enumerate() {
            let row = index * 64 + bit;
            if (selected >> bit) & 1 == 0 || row >= self.nrows() {
                continue;
            }
            *slot = self.values[key][row].unwrap_or(0);
            if self.values[key][row].is_some() {
                non_null |= 1 << bit;
            }
        }
        Ok(non_null)
    }
}

/// A generator of reproducible values.
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// A value of the kind, from a small range when `few`, else from the
    /// whole range with its edges likelier; NULL in about one row in
    /// `null_every` when that is not 0.
    fn value(&mut self, kind: KeyKind, few: bool, null_every: u64) -> Option<i64> {
        if null_every != 0 && self.next().is_multiple_of(null_every) {
            return None;
        }
        let raw = self.next();
        let value = match (kind, few, raw % 16) {
            (_, true, _) => (raw % 7) as i64 - 3,
            (KeyKind::Int32, false, 0) => i64::from(i32::MIN),
            (KeyKind::Int32, false, 1) => i64::from(i32::MAX),
            (KeyKind::Int32, false, _) => i64::from((raw >> 8) as i32),
            (KeyKind::Int64, false, 0) => i64::MIN,
            (KeyKind::Int64, false, 1) => i64::MAX,
            (KeyKind::Int64, false, _) => (raw >> 4) as i64 ^ (raw << 60) as i64,
        };
        Some(value)
    }
}

/// PostgreSQL's order of one key: NULL first or last whatever the
/// direction, values ascending or descending.
fn compare_key(key: &SortKey, a: Option<i64>, b: Option<i64>) -> Ordering {
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) if key.nulls_first => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) if key.nulls_first => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(a), Some(b)) if key.descending => b.cmp(&a),
        (Some(a), Some(b)) => a.cmp(&b),
    }
}

/// Append `rows` to a table of their keys in batches of 64, sort its items
/// and return the rows in the order the references come back, with the
/// references of the rows by row.
fn sorted_rows(keys: &[SortKey], rows: &Rows) -> Result<(Vec<usize>, Vec<u32>)> {
    let nrows = rows.nrows();
    let config = TableConfig {
        keys: &rows.kinds,
        payload_size: 8,
    };
    let mut table = LocalTable::new(&config, 0, CHUNK)?;
    let mut references = vec![0u32; nrows];
    let hashes = vec![0u32; nrows];
    let mut pending_words: Vec<u64> = vec![0; nrows.div_ceil(64)];
    for first in (0..nrows).step_by(64) {
        for row in first..(first + 64).min(nrows) {
            pending_words[row / 64] |= 1 << (row % 64);
        }
        if table.chunks() == 0 {
            table.add_chunk()?;
        }
        loop {
            let chunk = table.chunks() - 1;
            {
                let shared = table.table()?;
                let mut pending = RowMask::try_new(nrows, &mut pending_words)?;
                shared.append(chunk, &hashes, rows, None, &mut pending, &mut references)?;
            }
            if pending_words.iter().all(|&word| word == 0) {
                break;
            }
            table.add_chunk()?;
        }
    }
    let words = item_words(keys)?;
    let mut items = vec![0u64; nrows * words];
    let count = table.table()?.sort_items(keys, &mut items)?;
    assert_eq!(count, nrows, "every record makes an item");
    let mut out = vec![0u32; nrows];
    sort_items(&mut items, words, &mut out)?;
    // Rows are appended in order, chunk after chunk: their references rise
    // with the row, and a binary search finds a reference's row.
    assert!(references.is_sorted(), "references rise with the rows");
    let row_of = |reference: u32| references.binary_search(&reference).unwrap();
    Ok((out.into_iter().map(row_of).collect(), references))
}

/// Sort random rows by `keys` and check the order against the model: the
/// keys by PostgreSQL's rules, then the reference, which the items end
/// with.
fn check(keys: &[SortKey], nrows: usize, few: bool, seed: u64) -> Result<()> {
    let mut random = Random(seed);
    let rows = Rows {
        kinds: keys.iter().map(|key| key.kind).collect(),
        values: keys
            .iter()
            .map(|key| {
                (0..nrows)
                    .map(|_| random.value(key.kind, few, if key.nullable { 5 } else { 0 }))
                    .collect()
            })
            .collect(),
    };
    let (got, references) = sorted_rows(keys, &rows)?;
    let mut expected: Vec<usize> = (0..nrows).collect();
    expected.sort_by(|&a, &b| {
        keys.iter()
            .enumerate()
            .map(|(k, key)| compare_key(key, rows.values[k][a], rows.values[k][b]))
            .find(|order| order.is_ne())
            .unwrap_or_else(|| references[a].cmp(&references[b]))
    });
    assert_eq!(got, expected, "keys {keys:?}, {nrows} rows, few {few}");
    Ok(())
}

fn key(kind: KeyKind, descending: bool, nulls_first: bool, nullable: bool) -> SortKey {
    SortKey {
        kind,
        descending,
        nulls_first,
        nullable,
    }
}

#[test]
fn one_key_every_direction_and_null_place() -> Result<()> {
    for kind in [KeyKind::Int32, KeyKind::Int64] {
        for descending in [false, true] {
            for nulls_first in [false, true] {
                for nullable in [false, true] {
                    for (nrows, few) in [(1, false), (64, false), (65, true), (1000, false)] {
                        check(
                            &[key(kind, descending, nulls_first, nullable)],
                            nrows,
                            few,
                            7 + nrows as u64,
                        )?;
                    }
                }
            }
        }
    }
    Ok(())
}

#[test]
fn several_keys_across_word_boundaries() -> Result<()> {
    use KeyKind::{Int32, Int64};
    let sets: [&[SortKey]; 6] = [
        // Two int4 keys and a reference: one word.
        &[
            key(Int32, false, false, false),
            key(Int32, true, false, false),
        ],
        // Two nullable int4 keys: 66 bits and a reference, two words.
        &[
            key(Int32, false, true, true),
            key(Int32, false, false, true),
        ],
        // Three int4 keys, exactly two words.
        &[
            key(Int32, false, false, false),
            key(Int32, true, false, false),
            key(Int32, false, false, false),
        ],
        // An int8 and an int4, both nullable: keys cross the first word.
        &[key(Int64, true, true, true), key(Int32, false, false, true)],
        // Two nullable int8 keys: three words.
        &[key(Int64, false, false, true), key(Int64, true, true, true)],
        // Four keys of both kinds.
        &[
            key(Int32, false, false, true),
            key(Int64, false, true, true),
            key(Int32, true, true, false),
            key(Int64, true, false, true),
        ],
    ];
    for (index, keys) in sets.iter().enumerate() {
        // Few values, so that the later keys decide.
        check(keys, 500, true, 100 + index as u64)?;
        check(keys, 500, false, 200 + index as u64)?;
    }
    Ok(())
}

#[test]
fn item_widths() -> Result<()> {
    use KeyKind::{Int32, Int64};
    assert_eq!(item_words(&[key(Int32, false, false, false)])?, 1);
    assert_eq!(item_words(&[key(Int32, false, false, true)])?, 2);
    assert_eq!(item_words(&[key(Int64, false, false, false)])?, 2);
    assert_eq!(
        item_words(&[key(Int64, false, false, true); 16])?,
        MAX_ITEM_WORDS
    );
    assert!(item_words(&[]).is_err());
    assert!(item_words(&[key(Int32, false, false, false); 17]).is_err());
    Ok(())
}

#[test]
fn misuse_is_an_error() -> Result<()> {
    use KeyKind::{Int32, Int64};
    // A NULL in a key declared not nullable.
    let rows = Rows {
        kinds: vec![Int32],
        values: vec![vec![Some(1), None]],
    };
    assert!(sorted_rows(&[key(Int32, false, false, false)], &rows).is_err());
    // Keys that are not the table's.
    let rows = Rows {
        kinds: vec![Int32],
        values: vec![vec![Some(1)]],
    };
    assert!(sorted_rows(&[key(Int64, false, false, false)], &rows).is_err());
    // Items that do not hold the records, and outputs of the wrong size.
    let config = TableConfig {
        keys: &[Int32],
        payload_size: 8,
    };
    let table = LocalTable::new(&config, 0, CHUNK)?;
    let keys = [key(Int32, false, false, false)];
    assert_eq!(table.table()?.sort_items(&keys, &mut [])?, 0);
    let mut items = [0u64; 4];
    assert!(sort_items(&mut items, 1, &mut [0u32; 3]).is_err());
    assert!(sort_items(&mut items, 3, &mut [0u32; 1]).is_err());
    assert!(sort_items(&mut items, 0, &mut []).is_err());
    assert!(sort_items(&mut items, MAX_ITEM_WORDS + 1, &mut []).is_err());
    Ok(())
}

/// The best `n` rows of `rows` by `keys` through a top-N heap: batches of
/// 64 rows, each narrowed to the candidates once the heap is full, the
/// candidates appended and pushed; their keys in order.
fn top_rows(keys: &[SortKey], rows: &Rows, n: usize) -> Result<Vec<Vec<Option<i64>>>> {
    use tessera_kernels::sort::top_candidates;
    let nrows = rows.nrows();
    let config = TableConfig {
        keys: &rows.kinds,
        payload_size: 8,
    };
    let mut table = LocalTable::new(&config, 0, CHUNK)?;
    let words = item_words(keys)?;
    let mut heap = vec![0u64; n * words];
    let mut len = 0;
    let mut references = vec![0u32; nrows];
    let mut appended: Vec<(u32, usize)> = Vec::new();
    let hashes = vec![0u32; nrows];
    for first in (0..nrows).step_by(64) {
        let mut mask_words = vec![0u64; nrows.div_ceil(64)];
        for row in first..(first + 64).min(nrows) {
            mask_words[row / 64] |= 1 << (row % 64);
        }
        if len == n && n > 0 {
            let worst = heap[..words].to_vec();
            let mut mask = RowMask::try_new(nrows, &mut mask_words)?;
            top_candidates(keys, rows, &mut mask, &worst)?;
        } else if n == 0 {
            continue;
        }
        let candidates = mask_words.clone();
        if table.chunks() == 0 {
            table.add_chunk()?;
        }
        let mut pending_words = candidates.clone();
        loop {
            let chunk = table.chunks() - 1;
            {
                let shared = table.table()?;
                let mut pending = RowMask::try_new(nrows, &mut pending_words)?;
                shared.append(chunk, &hashes, rows, None, &mut pending, &mut references)?;
            }
            if pending_words.iter().all(|&word| word == 0) {
                break;
            }
            table.add_chunk()?;
        }
        for row in first..(first + 64).min(nrows) {
            if (candidates[row / 64] >> (row % 64)) & 1 == 1 {
                appended.push((references[row], row));
            }
        }
        let view = tessera_core::RowMaskView::try_new(nrows, &candidates)?;
        table
            .table()?
            .top_push(keys, &references, &view, &mut heap, &mut len)?;
    }
    let mut out = vec![0u32; len];
    sort_items(&mut heap[..len * words], words, &mut out)?;
    // Only the candidates were appended: their references, row by row.
    appended.sort_unstable();
    Ok(out
        .iter()
        .map(|reference| {
            let at = appended
                .binary_search_by_key(reference, |&(r, _)| r)
                .unwrap();
            let row = appended[at].1;
            (0..keys.len()).map(|key| rows.values[key][row]).collect()
        })
        .collect())
}

#[test]
fn a_top_n_heap_keeps_the_first_rows_in_order() -> Result<()> {
    use KeyKind::{Int32, Int64};
    let sets: [&[SortKey]; 4] = [
        &[key(Int32, false, false, true)],
        &[key(Int64, true, true, true)],
        &[key(Int32, false, true, true), key(Int64, true, false, true)],
        &[
            key(Int32, true, false, true),
            key(Int32, false, false, true),
        ],
    ];
    for (index, keys) in sets.iter().enumerate() {
        for (nrows, few) in [(500, false), (500, true), (130, false)] {
            let mut random = Random(300 + index as u64 + nrows as u64);
            let rows = Rows {
                kinds: keys.iter().map(|key| key.kind).collect(),
                values: keys
                    .iter()
                    .map(|key| (0..nrows).map(|_| random.value(key.kind, few, 5)).collect())
                    .collect(),
            };
            let mut all: Vec<Vec<Option<i64>>> = (0..nrows)
                .map(|row| (0..keys.len()).map(|key| rows.values[key][row]).collect())
                .collect();
            all.sort_by(|a, b| {
                keys.iter()
                    .enumerate()
                    .map(|(k, key)| compare_key(key, a[k], b[k]))
                    .find(|order| order.is_ne())
                    .unwrap_or(Ordering::Equal)
            });
            for n in [0, 1, 5, 64, 100, nrows, nrows + 10] {
                let got = top_rows(keys, &rows, n)?;
                assert_eq!(
                    got,
                    all[..n.min(nrows)],
                    "keys {keys:?}, {nrows} rows, few {few}, top {n}"
                );
            }
        }
    }
    Ok(())
}

#[test]
fn a_heap_rejects_the_rows_that_do_not_beat_its_worst() -> Result<()> {
    use tessera_kernels::sort::top_candidates;
    let keys = [key(KeyKind::Int32, false, false, true)];
    let rows = Rows {
        kinds: vec![KeyKind::Int32],
        values: vec![vec![Some(1), Some(5), None, Some(4), Some(-3)]],
    };
    let words = item_words(&keys)?;
    // A worst item of key 4: the NULL bit clear, the value's sign flipped.
    let mut worst = vec![0u64; words];
    worst[0] = (u64::from(4u32 ^ 0x8000_0000)) << 31;
    let mut mask_words = vec![0b11111u64];
    let mut mask = RowMask::try_new(5, &mut mask_words)?;
    assert_eq!(top_candidates(&keys, &rows, &mut mask, &worst)?, 2);
    assert_eq!(
        mask_words[0], 0b10001,
        "1 and -3 beat 4; 4 ties it; NULL and 5 lose"
    );
    let mut short = vec![0b1u64];
    let mut mask = RowMask::try_new(5, &mut short)?;
    assert!(top_candidates(&keys, &rows, &mut mask, &worst[..words - 1]).is_err());
    Ok(())
}

#[test]
fn key_lanes_order_the_selected_rows_as_their_keys() -> Result<()> {
    use KeyKind::{Int32, Int64};
    use tessera_kernels::sort::key_lanes;
    let sets: [&[SortKey]; 3] = [
        &[key(Int32, false, false, true)],
        &[key(Int64, true, true, true), key(Int32, false, false, true)],
        &[
            key(Int32, true, false, true),
            key(Int64, false, true, true),
            key(Int64, true, true, true),
        ],
    ];
    for (set, keys) in sets.iter().enumerate() {
        let nrows = 150;
        let mut random = Random(11 + set as u64);
        let rows = Rows {
            kinds: keys.iter().map(|key| key.kind).collect(),
            values: keys
                .iter()
                .map(|key| {
                    (0..nrows)
                        .map(|_| random.value(key.kind, true, 5))
                        .collect()
                })
                .collect(),
        };
        let mut mask_words = vec![random.next(), random.next(), random.next() & 0x3F_FFFF];
        let selected: Vec<usize> = (0..nrows)
            .filter(|row| (mask_words[row / 64] >> (row % 64)) & 1 == 1)
            .collect();
        let mask = RowMask::try_new(nrows, &mut mask_words)?;
        let words = item_words(keys)?;
        let mut storage = vec![vec![0u64; nrows]; words];
        let mut lanes: Vec<&mut [u64]> = storage.iter_mut().map(Vec::as_mut_slice).collect();
        assert_eq!(
            key_lanes(keys, &rows, &mask.as_view(), &mut lanes)?,
            selected.len()
        );
        let item = |at: usize| -> Vec<u64> { storage.iter().map(|lane| lane[at]).collect() };
        for a in 0..selected.len() {
            for b in 0..selected.len() {
                let expected = keys
                    .iter()
                    .enumerate()
                    .map(|(k, key)| {
                        compare_key(
                            key,
                            rows.values[k][selected[a]],
                            rows.values[k][selected[b]],
                        )
                    })
                    .find(|order| order.is_ne())
                    .unwrap_or(Ordering::Equal);
                assert_eq!(
                    item(a).cmp(&item(b)),
                    expected,
                    "keys {keys:?}, rows {a} and {b}"
                );
            }
        }
        let mut short = vec![vec![0u64; selected.len() - 1]; words];
        let mut lanes: Vec<&mut [u64]> = short.iter_mut().map(Vec::as_mut_slice).collect();
        assert!(key_lanes(keys, &rows, &mask.as_view(), &mut lanes).is_err());
    }
    Ok(())
}
