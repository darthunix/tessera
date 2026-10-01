#![forbid(unsafe_code)]

use std::cmp::Ordering;

use anyhow::Result;
use proptest::collection::vec;
use proptest::prelude::*;
use proptest::sample::select;
use tessera_core::RowMask;
use tessera_kernels::sort::{MAX_ITEM_WORDS, SortKey, item_words, sort_items};
use tessera_kernels::table::{Batch, KeyKind, KeySource, LocalTable, TableConfig};
use tessera_testing::{flags, integer, nrows, property, words};

/// Chunks of the tests: small, so that most row sets span several.
const CHUNK: usize = 4096;

/// Rows of keys, each one a value or NULL, as a key source.
#[derive(Clone, Debug)]
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

/// A key of either kind, direction and NULL place, nullable or not.
fn sort_key() -> impl Strategy<Value = SortKey> {
    (
        select(vec![KeyKind::Int32, KeyKind::Int64]),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(|(kind, descending, nulls_first, nullable)| {
            key(kind, descending, nulls_first, nullable)
        })
}

/// A value of the kind: from -3 to 3 when `few`, so that later keys
/// decide, else leaning to the edges of the kind; NULL one time in five
/// when the key is nullable.
fn value(key: &SortKey, few: bool) -> BoxedStrategy<Option<i64>> {
    let value = match (few, key.kind) {
        (true, _) => (-3_i64..=3).boxed(),
        (false, KeyKind::Int32) => integer::<i32>().prop_map(i64::from).boxed(),
        (false, KeyKind::Int64) => integer::<i64>(),
    };
    if key.nullable {
        prop_oneof![1 => Just(None), 4 => value.prop_map(Some)].boxed()
    } else {
        value.prop_map(Some).boxed()
    }
}

/// Rows for `keys`.
fn rows(keys: &[SortKey], nrows: usize, few: bool) -> impl Strategy<Value = Rows> + use<> {
    let kinds = keys.iter().map(|key| key.kind).collect();
    let values: Vec<_> = keys.iter().map(|key| vec(value(key, few), nrows)).collect();
    values.prop_map(move |values| Rows {
        kinds: Vec::clone(&kinds),
        values,
    })
}

/// Key sets of `nkeys` keys and rows for them: around word borders, or a
/// few hundred one time in four; few values or values leaning to the edges.
fn cases(nkeys: std::ops::RangeInclusive<usize>) -> impl Strategy<Value = (Vec<SortKey>, Rows)> {
    let sizes = prop_oneof![3 => nrows().prop_map(|nrows| nrows.max(1)), 1 => 300..=600_usize];
    (vec(sort_key(), nkeys), sizes, any::<bool>())
        .prop_flat_map(|(keys, nrows, few)| (Just(keys.clone()), rows(&keys, nrows, few)))
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
                shared.append(
                    chunk,
                    None,
                    &mut Batch::new(&hashes, rows, &mut pending, &mut references)?,
                )?;
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

/// Sort the rows by `keys` and check the order against the model: the
/// keys by PostgreSQL's rules, then the reference, which the items end
/// with.
fn check(keys: &[SortKey], rows: &Rows) -> Result<()> {
    let nrows = rows.nrows();
    let (got, references) = sorted_rows(keys, rows)?;
    let mut expected: Vec<usize> = (0..nrows).collect();
    expected.sort_by(|&a, &b| {
        keys.iter()
            .enumerate()
            .map(|(k, key)| compare_key(key, rows.values[k][a], rows.values[k][b]))
            .find(|order| order.is_ne())
            .unwrap_or_else(|| references[a].cmp(&references[b]))
    });
    assert_eq!(got, expected, "keys {keys:?}, {nrows} rows");
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
fn one_key_every_direction_and_null_place() {
    property(cases(1..=1), |(keys, rows)| check(&keys, &rows));
}

/// Two to four keys of both kinds: items of one to three words, keys that
/// cross word boundaries.
#[test]
fn several_keys_across_word_boundaries() {
    property(cases(2..=4), |(keys, rows)| check(&keys, &rows));
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
                shared.append(
                    chunk,
                    None,
                    &mut Batch::new(&hashes, rows, &mut pending, &mut references)?,
                )?;
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
fn a_top_n_heap_keeps_the_first_rows_in_order() {
    property(cases(1..=2), |(keys, rows)| -> Result<()> {
        let nrows = rows.nrows();
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
            let got = top_rows(&keys, &rows, n)?;
            assert_eq!(
                got,
                all[..n.min(nrows)],
                "keys {keys:?}, {nrows} rows, top {n}"
            );
        }
        Ok(())
    });
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
fn key_lanes_order_the_selected_rows_as_their_keys() {
    let cases = (vec(sort_key(), 1..=3), 1..=150_usize).prop_flat_map(|(keys, nrows)| {
        (Just(keys.clone()), rows(&keys, nrows, true), flags(nrows))
    });
    property(cases, |(keys, rows, selection)| -> Result<()> {
        key_lanes_order(&keys, &rows, &selection)
    });
}

fn key_lanes_order(keys: &[SortKey], rows: &Rows, selection: &[bool]) -> Result<()> {
    use tessera_kernels::sort::key_lanes;
    let nrows = rows.nrows();
    let mut mask_words = words(selection);
    let selected: Vec<usize> = (0..nrows).filter(|&row| selection[row]).collect();
    let mask = RowMask::try_new(nrows, &mut mask_words)?;
    let words = item_words(keys)?;
    let mut storage = vec![vec![0u64; nrows]; words];
    let mut lanes: Vec<&mut [u64]> = storage.iter_mut().map(Vec::as_mut_slice).collect();
    assert_eq!(
        key_lanes(keys, rows, &mask.as_view(), &mut lanes)?,
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
    if !selected.is_empty() {
        let mut short = vec![vec![0u64; selected.len() - 1]; words];
        let mut lanes: Vec<&mut [u64]> = short.iter_mut().map(Vec::as_mut_slice).collect();
        assert!(key_lanes(keys, rows, &mask.as_view(), &mut lanes).is_err());
    }
    Ok(())
}
