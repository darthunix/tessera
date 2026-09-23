#![forbid(unsafe_code)]

use anyhow::Result;
use tessera_core::{ColumnView, RowMask, RowMaskView};
use tessera_kernels::int32::{self, NullKeys, hash_combine, murmurhash32};
use tessera_kernels::table::{
    FORMAT_VERSION, KeyKind, KeySource, MAX_KEYS, TableConfig, TableMut, normalize_word,
    region_size,
};

/// Bytes of the header, as the format fixes it.
const HEADER: usize = 96;
/// Byte offsets of the header fields the tests corrupt.
const MAGIC: usize = 0;
const VERSION: usize = 8;
const HEADER_SIZE: usize = 12;
const REGION_LEN: usize = 16;
const BUCKETS_OFFSET: usize = 24;
const CHUNK_USED: usize = 32;
const NRECORDS: usize = 40;
const NBUCKETS: usize = 48;
const BUCKET_SHIFT: usize = 52;
const RECORD_SIZE: usize = 56;
const NKEYS: usize = 64;
const FLAGS: usize = 68;
const KINDS: usize = 72;

const ONE_INT4: TableConfig<'static> = TableConfig {
    keys: &[KeyKind::Int32],
    payload_size: 8,
};

/// A named way to damage a header.
type Corruption<'a> = (&'a str, &'a dyn Fn(&mut [u64]));
/// An insertion with something wrong about its arguments.
type Attempt<'a> = &'a dyn Fn(&mut RowMask<'_>, &mut [u32]) -> Result<usize>;

/// A zeroed region for `capacity` records of `config`.
fn words_for(config: &TableConfig<'_>, capacity: u64) -> Result<Vec<u64>> {
    Ok(vec![0; region_size(config, capacity)?.div_ceil(8)])
}

/// The bytes of one field, whatever the word boundaries.
fn set_bytes(words: &mut [u64], offset: usize, bytes: &[u8]) {
    for (i, &byte) in bytes.iter().enumerate() {
        let at = offset + i;
        let mut word = words[at / 8].to_ne_bytes();
        word[at % 8] = byte;
        words[at / 8] = u64::from_ne_bytes(word);
    }
}

fn set_u32(words: &mut [u64], offset: usize, value: u32) {
    set_bytes(words, offset, &value.to_ne_bytes());
}

fn set_u64(words: &mut [u64], offset: usize, value: u64) {
    set_bytes(words, offset, &value.to_ne_bytes());
}

/// The record size of the format: header, key slots and payload, rounded
/// up to 8.
fn model_record_size(nkeys: usize, payload_size: usize) -> usize {
    (16 + 8 * nkeys + payload_size + 7) & !7
}

/// The bucket count of the format: a power of two, at least 1024 and at
/// least twice the capacity.
fn model_buckets(capacity: u64) -> u64 {
    (capacity * 2).max(1024).next_power_of_two()
}

#[test]
fn a_created_table_is_empty_and_attaches_again() -> Result<()> {
    let config = TableConfig {
        keys: &[KeyKind::Int32, KeyKind::Int64],
        payload_size: 12,
    };
    let mut words = words_for(&config, 300)?;
    let len = words.len() * 8;
    let stats = {
        let table = TableMut::create_in(&mut words, &config, 300)?;
        let stats = table.stats();
        assert_eq!(stats.records, 0);
        assert_eq!(stats.buckets, 1024);
        assert_eq!(stats.bytes_used, HEADER as u64 + 1024 * 4);
        assert_eq!(stats.region_len, len as u64);
        assert_eq!(table.key_kinds(), config.keys);
        assert_eq!(table.payload_size(), 12);
        stats
    };
    let again = TableMut::exclusive(&mut words)?;
    assert_eq!(again.stats(), stats);
    assert_eq!(again.key_kinds(), config.keys);
    Ok(())
}

#[test]
fn a_larger_region_than_needed_is_used_whole() -> Result<()> {
    let mut words = words_for(&ONE_INT4, 10)?;
    words.extend([0; 1000]);
    let len = words.len() * 8;
    let table = TableMut::create_in(&mut words, &ONE_INT4, 10)?;
    assert_eq!(table.stats().region_len, len as u64);
    assert_eq!(table.stats().buckets, 1024);
    Ok(())
}

#[test]
fn region_size_covers_header_records_and_buckets() -> Result<()> {
    let keys = [KeyKind::Int32; 3];
    for payload_size in [0, 1, 8, 13] {
        let config = TableConfig {
            keys: &keys,
            payload_size,
        };
        for capacity in [0, 1, 511, 512, 513, 1000, 100_000] {
            let size = region_size(&config, capacity)?;
            let expected = HEADER
                + capacity as usize * model_record_size(3, payload_size)
                + model_buckets(capacity) as usize * 4;
            assert_eq!(size, expected, "{payload_size} payload, {capacity} records");
            assert_eq!(size % 8, 0);
            let mut words = vec![0; size / 8];
            assert!(TableMut::create_in(&mut words, &config, capacity).is_ok());
        }
    }
    Ok(())
}

#[test]
fn a_tight_region_is_refused() -> Result<()> {
    let mut words = words_for(&ONE_INT4, 100)?;
    let short = words.len() - 1;
    let error = TableMut::create_in(&mut words[..short], &ONE_INT4, 100).unwrap_err();
    assert!(error.to_string().contains("smaller"), "{error}");
    assert!(TableMut::create_in(&mut words, &ONE_INT4, 100).is_ok());
    Ok(())
}

#[test]
fn configurations_are_checked() -> Result<()> {
    let mut words = vec![0; 2048];
    let no_keys = TableConfig {
        keys: &[],
        payload_size: 0,
    };
    assert!(region_size(&no_keys, 1).is_err());
    assert!(TableMut::create_in(&mut words, &no_keys, 1).is_err());
    let too_many = TableConfig {
        keys: &[KeyKind::Int64; MAX_KEYS + 1],
        payload_size: 0,
    };
    assert!(region_size(&too_many, 1).is_err());
    let most = TableConfig {
        keys: &[KeyKind::Int64; MAX_KEYS],
        payload_size: 0,
    };
    assert!(TableMut::create_in(&mut words, &most, 1).is_ok());
    let huge_payload = TableConfig {
        keys: &[KeyKind::Int32],
        payload_size: usize::MAX - 100,
    };
    assert!(region_size(&huge_payload, 1).is_err());
    let wide_payload = TableConfig {
        keys: &[KeyKind::Int32],
        payload_size: 1 << 33,
    };
    assert!(region_size(&wide_payload, 1).is_err());
    assert!(region_size(&ONE_INT4, u64::MAX).is_err());
    assert!(region_size(&ONE_INT4, u64::MAX / 32).is_err());
    Ok(())
}

#[test]
fn a_slice_shorter_than_the_region_is_refused() -> Result<()> {
    let mut words = words_for(&ONE_INT4, 100)?;
    TableMut::create_in(&mut words, &ONE_INT4, 100)?;
    let short = words.len() - 1;
    let error = TableMut::exclusive(&mut words[..short]).unwrap_err();
    assert!(error.to_string().contains("exceeds"), "{error}");
    assert!(TableMut::exclusive(&mut words).is_ok());
    Ok(())
}

#[test]
fn a_corrupt_header_is_refused() -> Result<()> {
    let config = TableConfig {
        keys: &[KeyKind::Int32, KeyKind::Int64],
        payload_size: 8,
    };
    let fresh = |words: &mut Vec<u64>| -> Result<usize> {
        TableMut::create_in(words, &config, 100)?;
        Ok(words.len() * 8)
    };
    let mut words = words_for(&config, 100)?;
    let len = fresh(&mut words)?;
    let buckets_offset = (len - 1024 * 4) as u64;
    let corruptions: &[Corruption<'_>] = &[
        ("magic", &|w| set_u64(w, MAGIC, 0x5445_5353_5f54_4142)),
        ("version", &|w| set_u32(w, VERSION, FORMAT_VERSION + 1)),
        ("header size", &|w| set_u32(w, HEADER_SIZE, 88)),
        ("region longer than given", &|w| {
            set_u64(w, REGION_LEN, len as u64 + 8)
        }),
        ("region shorter than buckets", &|w| {
            set_u64(w, REGION_LEN, len as u64 - 8)
        }),
        ("region not a multiple of 8", &|w| {
            set_u64(w, REGION_LEN, len as u64 - 4)
        }),
        ("buckets past the end", &|w| {
            set_u64(w, BUCKETS_OFFSET, buckets_offset + 8)
        }),
        ("buckets misaligned", &|w| {
            set_u64(w, BUCKETS_OFFSET, buckets_offset - 4)
        }),
        ("records overlap buckets", &|w| {
            set_u64(w, CHUNK_USED, buckets_offset + 8)
        }),
        ("record area before header", &|w| set_u64(w, CHUNK_USED, 88)),
        ("record area misaligned", &|w| set_u64(w, CHUNK_USED, 100)),
        ("more records than bytes", &|w| set_u64(w, NRECORDS, 1)),
        ("buckets not a power of two", &|w| {
            set_u32(w, NBUCKETS, 1000)
        }),
        ("too few buckets", &|w| set_u32(w, NBUCKETS, 512)),
        ("bucket shift", &|w| set_u32(w, BUCKET_SHIFT, 21)),
        ("record size", &|w| set_u32(w, RECORD_SIZE, 48)),
        ("no keys", &|w| set_u32(w, NKEYS, 0)),
        ("too many keys", &|w| set_u32(w, NKEYS, MAX_KEYS as u32 + 1)),
        ("unknown key kind", &|w| set_bytes(w, KINDS + 1, &[3])),
        ("flags", &|w| set_u32(w, FLAGS, 1)),
    ];
    for (what, corrupt) in corruptions {
        fresh(&mut words)?;
        corrupt(&mut words);
        assert!(
            TableMut::exclusive(&mut words).is_err(),
            "{what} was accepted"
        );
    }
    fresh(&mut words)?;
    set_bytes(&mut words, KINDS + 2, &[3]);
    assert!(
        TableMut::exclusive(&mut words).is_ok(),
        "a kind byte past the keys is ignored"
    );
    Ok(())
}

/// The int4 value that hashes like a NULL key under the group policy.
const GROUP_KEY: i32 = 0x9e37_79b9_u32 as i32;

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
    view.selected_indices().collect()
}

/// A payload of eight bytes per row: the row number times ten.
fn payload_for(nrows: usize) -> Vec<u8> {
    (0..nrows as u64)
        .flat_map(|row| (row * 10).to_ne_bytes())
        .collect()
}

/// A key column of int4 or int8 values.
enum Key<'a> {
    Int4(ColumnView<'a, i32>),
    Int8(ColumnView<'a, i64>),
}

/// Keys of mixed kinds: a wrapper on the caller's side is a key source.
struct Mixed<'a>(Vec<Key<'a>>);

impl KeySource for Mixed<'_> {
    fn nkeys(&self) -> usize {
        self.0.len()
    }

    fn nrows(&self) -> usize {
        match &self.0[0] {
            Key::Int4(column) => column.nrows(),
            Key::Int8(column) => column.nrows(),
        }
    }

    fn word(&self, key: usize, index: usize, selected: u64, out: &mut [i64; 64]) -> Result<u64> {
        match &self.0[key] {
            Key::Int4(column) => normalize_word(column, index, selected, out),
            Key::Int8(column) => normalize_word(column, index, selected, out),
        }
    }
}

/// Insert every row of a batch, expecting room for all of them.
fn insert_all<K: KeySource + ?Sized>(
    table: &TableMut<'_>,
    hashes: &[u32],
    keys: &K,
    payload: Option<&[u8]>,
) -> Result<Vec<u32>> {
    let nrows = hashes.len();
    let mut pending_words = all_rows(nrows);
    let mut pending = RowMask::try_new(nrows, &mut pending_words)?;
    let mut offsets = vec![0; nrows];
    let inserted = table.insert(hashes, keys, payload, &mut pending, &mut offsets)?;
    assert_eq!(inserted, nrows);
    assert_eq!(pending.as_view().selected_count(), 0);
    Ok(offsets)
}

#[test]
fn inserted_rows_get_their_records() -> Result<()> {
    let values: Vec<i32> = (0..200).collect();
    let keys = [ColumnView::try_new(&values, None)?];
    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
    let payload = payload_for(200);
    let mut words = words_for(&ONE_INT4, 200)?;
    let table = TableMut::create_in(&mut words, &ONE_INT4, 200)?;
    let offsets = insert_all(&table, &hashes, &keys[..], Some(&payload))?;
    assert_eq!(table.stats().records, 200);
    assert_eq!(
        table.stats().bytes_used,
        HEADER as u64 + 200 * 32 + 1024 * 4,
        "a record of one key and eight payload bytes takes 32 bytes"
    );
    let mut seen = offsets.clone();
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(seen.len(), 200, "every record has its own offset");
    for (row, &offset) in offsets.iter().enumerate() {
        let record = table.record(offset)?;
        assert_eq!(record.hash, hashes[row]);
        assert_eq!(record.null_bits, 0);
        assert_eq!(record.keys, &[row as i64]);
        assert_eq!(record.payload, (row as u64 * 10).to_ne_bytes());
    }
    Ok(())
}

#[test]
fn a_null_key_groups_apart_from_the_value_it_hashes_like() -> Result<()> {
    let values = [GROUP_KEY, 0];
    let non_null_words = [0b01];
    let keys = [ColumnView::try_new(
        &values,
        Some(RowMaskView::try_new(2, &non_null_words)?),
    )?];
    let mut hashes = [0; 2];
    let mut valid_words = [0b11];
    let mut valid = RowMask::try_new(2, &mut valid_words)?;
    let rows_words = [0b11];
    let rows = RowMaskView::try_new(2, &rows_words)?;
    int32::hash(&keys[0], &rows, NullKeys::Group, &mut hashes, &mut valid)?;
    assert_eq!(hashes[0], hashes[1], "the value and the NULL hash alike");
    assert_eq!(valid.as_view().selected_count(), 2);

    let mut words = words_for(&ONE_INT4, 2)?;
    let table = TableMut::create_in(&mut words, &ONE_INT4, 2)?;
    let mut offsets = [0; 2];
    table.insert(&hashes, &keys[..], None, &mut valid, &mut offsets)?;
    assert_eq!(table.stats().records, 2);
    assert_ne!(offsets[0], offsets[1], "each row has its own record");
    let value = table.record(offsets[0])?;
    assert_eq!(
        (value.null_bits, value.keys),
        (0, &[i64::from(GROUP_KEY)][..])
    );
    let null = table.record(offsets[1])?;
    assert_eq!((null.null_bits, null.keys), (1, &[0][..]));

    let mut rejected_words = [0b11];
    let mut rejected = RowMask::try_new(2, &mut rejected_words)?;
    int32::hash(
        &keys[0],
        &rows,
        NullKeys::Reject,
        &mut hashes,
        &mut rejected,
    )?;
    let mut words = words_for(&ONE_INT4, 2)?;
    let table = TableMut::create_in(&mut words, &ONE_INT4, 2)?;
    table.insert(&hashes, &keys[..], None, &mut rejected, &mut offsets)?;
    assert_eq!(
        table.stats().records,
        1,
        "the rejected NULL row is not inserted"
    );
    Ok(())
}

#[test]
fn two_keys_of_different_kinds_fill_their_slots() -> Result<()> {
    let config = TableConfig {
        keys: &[KeyKind::Int32, KeyKind::Int64],
        payload_size: 0,
    };
    let first: Vec<i32> = (0..100).map(|row| row % 7 - 3).collect();
    let second: Vec<i64> = (0..100).map(|row| row * 1_000_000_007 - 5).collect();
    let hashes: Vec<u32> = first
        .iter()
        .zip(&second)
        .map(|(&a, &b)| hash_combine(hash_i32(a), hash_i64(b)))
        .collect();
    let keys = Mixed(vec![
        Key::Int4(ColumnView::try_new(&first, None)?),
        Key::Int8(ColumnView::try_new(&second, None)?),
    ]);
    let mut words = words_for(&config, 100)?;
    let table = TableMut::create_in(&mut words, &config, 100)?;
    let offsets = insert_all(&table, &hashes, &keys, None)?;
    let record = table.record(offsets[5])?;
    assert_eq!(record.keys, &[i64::from(first[5]), second[5]]);
    assert!(record.payload.is_empty());
    Ok(())
}

#[test]
fn a_full_table_leaves_the_rest_pending() -> Result<()> {
    let values: Vec<i32> = (0..100).collect();
    let keys = [ColumnView::try_new(&values, None)?];
    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
    let mut words = words_for(&ONE_INT4, 16)?;
    let table = TableMut::create_in(&mut words, &ONE_INT4, 16)?;
    let mut pending_words = all_rows(100);
    let mut pending = RowMask::try_new(100, &mut pending_words)?;
    let mut offsets = vec![0; 100];
    let inserted = table.insert(&hashes, &keys[..], None, &mut pending, &mut offsets)?;
    assert_eq!(inserted, 16, "a region for 16 records holds 16");
    assert_eq!(rows_of(&pending.as_view()), (16..100).collect::<Vec<_>>());
    assert_eq!(table.stats().records, 16);
    assert_eq!(
        table.stats().bytes_used,
        table.stats().region_len,
        "no byte is left"
    );
    let again = table.insert(&hashes, &keys[..], None, &mut pending, &mut offsets)?;
    assert_eq!(again, 0);
    assert_eq!(pending.as_view().selected_count(), 84);
    Ok(())
}

#[test]
fn batches_of_every_shape_insert() -> Result<()> {
    for nrows in [0, 1, 63, 64, 65, 200, 1000] {
        let values: Vec<i32> = (0..nrows as i32).map(|row| row * 3 - 1000).collect();
        let keys = [ColumnView::try_new(&values, None)?];
        let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
        let payload = payload_for(nrows);
        let mut words = words_for(&ONE_INT4, nrows as u64)?;
        let table = TableMut::create_in(&mut words, &ONE_INT4, nrows as u64)?;
        let offsets = insert_all(&table, &hashes, &keys[..], Some(&payload))?;
        assert_eq!(table.stats().records, nrows as u64, "{nrows} rows");
        if nrows > 0 {
            let last = table.record(offsets[nrows - 1])?;
            assert_eq!(last.payload, ((nrows as u64 - 1) * 10).to_ne_bytes());
        }
    }
    Ok(())
}

#[test]
fn a_missing_payload_is_zeros() -> Result<()> {
    let values = [42];
    let keys = [ColumnView::try_new(&values, None)?];
    let hashes = [hash_i32(42)];
    let config = TableConfig {
        keys: &[KeyKind::Int32],
        payload_size: 13,
    };
    let mut words = words_for(&config, 1)?;
    let table = TableMut::create_in(&mut words, &config, 1)?;
    let offsets = insert_all(&table, &hashes, &keys[..], None)?;
    assert_eq!(table.record(offsets[0])?.payload, [0; 13]);
    Ok(())
}

#[test]
fn dimension_errors_come_before_any_change() -> Result<()> {
    let config = TableConfig {
        keys: &[KeyKind::Int32, KeyKind::Int32],
        payload_size: 8,
    };
    let values: Vec<i32> = (0..70).collect();
    let one_key = [ColumnView::try_new(&values, None)?];
    let two_keys = [
        ColumnView::try_new(&values, None)?,
        ColumnView::try_new(&values, None)?,
    ];
    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
    let payload = payload_for(70);
    let mut words = words_for(&config, 70)?;
    let table = TableMut::create_in(&mut words, &config, 70)?;
    let mut pending_words = all_rows(70);
    let mut offsets = vec![0; 70];
    {
        let mut pending = RowMask::try_new(70, &mut pending_words)?;
        let attempts: [Attempt<'_>; 4] = [
            &|pending, offsets| table.insert(&hashes, &one_key[..], None, pending, offsets),
            &|pending, offsets| table.insert(&hashes[..69], &two_keys[..], None, pending, offsets),
            &|pending, offsets| {
                table.insert(&hashes, &two_keys[..], None, pending, &mut offsets[..69])
            },
            &|pending, offsets| {
                table.insert(
                    &hashes,
                    &two_keys[..],
                    Some(&payload[..8]),
                    pending,
                    offsets,
                )
            },
        ];
        for (what, attempt) in attempts.iter().enumerate() {
            assert!(
                attempt(&mut pending, &mut offsets).is_err(),
                "attempt {what}"
            );
        }
        assert_eq!(pending.as_view().selected_count(), 70);
    }
    assert_eq!(table.stats().records, 0);
    let mut pending = RowMask::try_new(70, &mut pending_words)?;
    assert_eq!(
        table.insert(
            &hashes,
            &two_keys[..],
            Some(&payload),
            &mut pending,
            &mut offsets
        )?,
        70
    );
    Ok(())
}

#[test]
fn an_offset_inside_a_record_is_refused() -> Result<()> {
    let values: Vec<i32> = (0..5).collect();
    let keys = [ColumnView::try_new(&values, None)?];
    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
    let mut words = words_for(&ONE_INT4, 5)?;
    let table = TableMut::create_in(&mut words, &ONE_INT4, 5)?;
    let offsets = insert_all(&table, &hashes, &keys[..], None)?;
    assert!(table.record(offsets[0]).is_ok());
    assert!(table.record(offsets[0] + 1).is_err(), "inside a record");
    assert!(table.record(0).is_err(), "the header");
    assert!(table.record(1).is_err(), "inside the header");
    let past = table.stats().bytes_used / 8;
    assert!(table.record(past as u32).is_err(), "past the records");
    Ok(())
}
