#![forbid(unsafe_code)]

use anyhow::Result;
use tessera_core::{ColumnReader, ColumnView, RowMask, RowMaskView};
use tessera_kernels::int32::{self, NullKeys, hash_combine, murmurhash32};
use tessera_kernels::table::{
    Cursor, FORMAT_VERSION, KeyKind, KeySource, MAX_KEYS, TableConfig, TableMut, normalize_word,
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

/// Probe every row of a batch: the rows found and their matches.
fn probe_all<K: KeySource + ?Sized>(
    table: &TableMut<'_>,
    hashes: &[u32],
    keys: &K,
) -> Result<(Vec<usize>, Vec<u32>)> {
    let nrows = hashes.len();
    let rows_words = all_rows(nrows);
    let rows = RowMaskView::try_new(nrows, &rows_words)?;
    let mut found_words = vec![0; nrows.div_ceil(64)];
    let mut found = RowMask::try_new(nrows, &mut found_words)?;
    let mut matches = vec![0; nrows];
    table.probe(hashes, keys, &rows, &mut matches, &mut found)?;
    Ok((rows_of(&found.as_view()), matches))
}

#[test]
fn inserted_rows_are_found_and_absent_keys_are_not() -> Result<()> {
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
    let (found, matches) = probe_all(&table, &hashes, &keys[..])?;
    assert_eq!(found, (0..200).collect::<Vec<_>>());
    assert_eq!(matches, offsets);

    let absent: Vec<i32> = (1000..1200).collect();
    let absent_keys = [ColumnView::try_new(&absent, None)?];
    let absent_hashes: Vec<u32> = absent.iter().map(|&value| hash_i32(value)).collect();
    let (found, _) = probe_all(&table, &absent_hashes, &absent_keys[..])?;
    assert!(found.is_empty());

    let every_third: Vec<u64> = all_rows(200)
        .iter()
        .map(|word| word & 0x9249_2492_4924_9249)
        .collect();
    let rows = RowMaskView::try_new(200, &every_third)?;
    let mut found_words = all_rows(200);
    let mut found = RowMask::try_new(200, &mut found_words)?;
    let mut matches = vec![0; 200];
    assert_eq!(found.as_view().selected_count(), 200);
    table.probe(&hashes, &keys[..], &rows, &mut matches, &mut found)?;
    assert_eq!(
        rows_of(&found.as_view()),
        rows_of(&rows),
        "the result is rewritten whole"
    );
    for row in rows_of(&rows) {
        assert_eq!(matches[row], offsets[row]);
    }
    Ok(())
}

#[test]
fn equal_keys_chain_through_next_match() -> Result<()> {
    let values: Vec<i32> = (0..100).map(|row| row % 10).collect();
    let keys = [ColumnView::try_new(&values, None)?];
    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
    let payload = payload_for(100);
    let mut words = words_for(&ONE_INT4, 100)?;
    let table = TableMut::create_in(&mut words, &ONE_INT4, 100)?;
    insert_all(&table, &hashes, &keys[..], Some(&payload))?;

    let (found, matches) = probe_all(&table, &hashes, &keys[..])?;
    assert_eq!(found.len(), 100);
    let mut chains: Vec<Vec<u32>> = (0..10).map(|key| vec![matches[key]]).collect();
    let mut current = matches[..10].to_vec();
    let mut rows_words = all_rows(10);
    loop {
        let rows = RowMaskView::try_new(10, &rows_words)?;
        let mut more_words = [0];
        let mut more = RowMask::try_new(10, &mut more_words)?;
        table.next_match(&mut current, &rows, &mut more)?;
        let rows = rows_of(&more.as_view());
        if rows.is_empty() {
            break;
        }
        for &key in &rows {
            chains[key].push(current[key]);
        }
        rows_words = vec![more_words[0]];
    }
    for (key, chain) in chains.iter().enumerate() {
        assert_eq!(chain.len(), 10, "key {key} has ten records");
        let mut rows: Vec<u64> = chain
            .iter()
            .map(|&offset| {
                let record = table.record(offset).unwrap();
                assert_eq!(record.keys, &[key as i64]);
                u64::from_ne_bytes(record.payload.try_into().unwrap()) / 10
            })
            .collect();
        rows.sort_unstable();
        assert_eq!(
            rows,
            (0..10).map(|i| (key + 10 * i) as u64).collect::<Vec<_>>()
        );
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
    let (found, matches) = probe_all(&table, &hashes, &keys[..])?;
    assert_eq!(found, [0, 1]);
    assert_ne!(matches[0], matches[1], "each row finds its own record");
    let value = table.record(matches[0])?;
    assert_eq!(
        (value.null_bits, value.keys),
        (0, &[i64::from(GROUP_KEY)][..])
    );
    let null = table.record(matches[1])?;
    assert_eq!((null.null_bits, null.keys), (1, &[0][..]));
    let mut more_words = [0];
    let mut more = RowMask::try_new(2, &mut more_words)?;
    let mut next = matches.clone();
    table.next_match(&mut next, &rows, &mut more)?;
    assert_eq!(more_words, [0], "neither has a second record");
    assert_eq!(next, matches, "offsets without a next record stay");

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
    let (found, _) = probe_all(&table, &hashes, &keys[..])?;
    assert_eq!(found, [0]);
    Ok(())
}

#[test]
fn two_keys_of_different_kinds_compare_whole() -> Result<()> {
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
    let (found, matches) = probe_all(&table, &hashes, &keys)?;
    assert_eq!(found.len(), 100);
    assert_eq!(matches, offsets);

    let other_second: Vec<i64> = second.iter().map(|value| value + 1).collect();
    let keys = Mixed(vec![
        Key::Int4(ColumnView::try_new(&first, None)?),
        Key::Int8(ColumnView::try_new(&other_second, None)?),
    ]);
    let (found, _) = probe_all(&table, &hashes, &keys)?;
    assert!(
        found.is_empty(),
        "the same hash and first key are not enough"
    );
    let other_first: Vec<i32> = first.iter().map(|value| value + 1).collect();
    let keys = Mixed(vec![
        Key::Int4(ColumnView::try_new(&other_first, None)?),
        Key::Int8(ColumnView::try_new(&second, None)?),
    ]);
    let (found, _) = probe_all(&table, &hashes, &keys)?;
    assert!(found.is_empty());
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
    let (found, matches) = probe_all(&table, &hashes, &keys[..])?;
    assert_eq!(found, (0..16).collect::<Vec<_>>());
    assert_eq!(matches[..16], offsets[..16]);
    Ok(())
}

#[test]
fn batches_of_every_shape_round_trip() -> Result<()> {
    for nrows in [0, 1, 63, 64, 65, 200, 1000] {
        let values: Vec<i32> = (0..nrows as i32).map(|row| row * 3 - 1000).collect();
        let keys = [ColumnView::try_new(&values, None)?];
        let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
        let payload = payload_for(nrows);
        let mut words = words_for(&ONE_INT4, nrows as u64)?;
        let table = TableMut::create_in(&mut words, &ONE_INT4, nrows as u64)?;
        let offsets = insert_all(&table, &hashes, &keys[..], Some(&payload))?;
        assert_eq!(table.stats().records, nrows as u64, "{nrows} rows");
        let (found, matches) = probe_all(&table, &hashes, &keys[..])?;
        assert_eq!(found, (0..nrows).collect::<Vec<_>>(), "{nrows} rows");
        assert_eq!(matches, offsets, "{nrows} rows");
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
    let rows_words = all_rows(70);
    let rows = RowMaskView::try_new(70, &rows_words)?;
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
    let mut found_words = vec![0; 2];
    let mut found = RowMask::try_new(70, &mut found_words)?;
    let mut matches = vec![0; 70];
    assert!(
        table
            .probe(&hashes, &one_key[..], &rows, &mut matches, &mut found)
            .is_err()
    );
    assert!(
        table
            .probe(
                &hashes,
                &two_keys[..],
                &rows,
                &mut matches[..69],
                &mut found
            )
            .is_err()
    );
    let mut short_words = vec![0; 2];
    let mut short = RowMask::try_new(69, &mut short_words)?;
    assert!(
        table
            .probe(&hashes, &two_keys[..], &rows, &mut matches, &mut short)
            .is_err()
    );
    assert!(
        table
            .next_match(&mut offsets[..69], &rows, &mut found)
            .is_err()
    );
    assert!(table.next_match(&mut offsets, &rows, &mut short).is_err());
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
fn a_corrupt_bucket_or_chain_is_an_error() -> Result<()> {
    let values: Vec<i32> = (0..5).collect();
    let keys = [ColumnView::try_new(&values, None)?];
    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
    let mut words = words_for(&ONE_INT4, 5)?;
    let len = words.len() * 8;
    let offsets = {
        let table = TableMut::create_in(&mut words, &ONE_INT4, 5)?;
        insert_all(&table, &hashes, &keys[..], None)?
    };
    let record_byte = offsets[0] as usize * 8;
    assert!(table_probe_one(&mut words, &hashes, &keys).is_ok());

    // The head of the bucket of key 0 points past the records.
    let bucket = len - 1024 * 4 + (hashes[0] >> 22) as usize * 4;
    let mut damaged = words.clone();
    set_u32(&mut damaged, bucket, (len / 8) as u32);
    assert!(table_probe_one(&mut damaged, &hashes, &keys).is_err());
    let mut damaged = words.clone();
    set_u32(&mut damaged, bucket, 1);
    assert!(table_probe_one(&mut damaged, &hashes, &keys).is_err());
    let mut damaged = words.clone();
    set_u32(&mut damaged, bucket, offsets[0] + 1);
    assert!(table_probe_one(&mut damaged, &hashes, &keys).is_err());
    {
        let table = TableMut::exclusive(&mut words)?;
        assert!(
            table.record(offsets[0] + 1).is_err(),
            "an offset inside a record"
        );
    }

    // The record of key 0 chains to itself: a probe with its hash and an
    // absent key walks the cycle and gives up.
    set_u32(&mut words, record_byte + 4, offsets[0]);
    let absent = [999; 5];
    let absent_keys = [ColumnView::try_new(&absent, None)?];
    let error = table_probe_one(&mut words, &hashes, &absent_keys).unwrap_err();
    assert!(error.to_string().contains("longer"), "{error}");
    Ok(())
}

/// Probe the first row of a batch through a fresh attachment.
fn table_probe_one(
    words: &mut [u64],
    hashes: &[u32],
    keys: &[ColumnView<'_, i32>],
) -> Result<bool> {
    let nrows = hashes.len();
    let table = TableMut::exclusive(words)?;
    let rows_words = [1];
    let rows = RowMaskView::try_new(nrows, &rows_words)?;
    let mut found_words = [0];
    let mut found = RowMask::try_new(nrows, &mut found_words)?;
    let mut matches = vec![0; nrows];
    table.probe(hashes, keys, &rows, &mut matches, &mut found)?;
    Ok(found_words[0] == 1)
}

/// Resolve every row of a batch to a record, expecting room for all: the
/// offsets and the rows whose record was created.
fn resolve_all<K: KeySource + ?Sized>(
    table: &mut TableMut<'_>,
    hashes: &[u32],
    keys: &K,
) -> Result<(Vec<u32>, Vec<usize>)> {
    let nrows = hashes.len();
    let mut pending_words = all_rows(nrows);
    let mut pending = RowMask::try_new(nrows, &mut pending_words)?;
    let mut offsets = vec![0; nrows];
    let mut inserted_words = vec![0; nrows.div_ceil(64)];
    let mut inserted = RowMask::try_new(nrows, &mut inserted_words)?;
    let resolved = table.find_or_insert(hashes, keys, &mut pending, &mut offsets, &mut inserted)?;
    assert_eq!(resolved, nrows);
    assert_eq!(pending.as_view().selected_count(), 0);
    let created = rows_of(&inserted.as_view());
    Ok((offsets, created))
}

/// Every record of a table in insertion order, through walks of `step`.
fn scan_all(table: &TableMut<'_>, step: usize) -> Result<Vec<u32>> {
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

#[test]
fn rows_find_or_create_the_record_of_their_keys() -> Result<()> {
    let values: Vec<i32> = (0..100).map(|row| row % 10).collect();
    let keys = [ColumnView::try_new(&values, None)?];
    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
    let mut words = words_for(&ONE_INT4, 20)?;
    let mut table = TableMut::create_in(&mut words, &ONE_INT4, 20)?;
    let (offsets, created) = resolve_all(&mut table, &hashes, &keys[..])?;
    assert_eq!(
        created,
        (0..10).collect::<Vec<_>>(),
        "the first row of each key"
    );
    assert_eq!(table.stats().records, 10);
    for row in 0..100 {
        assert_eq!(offsets[row], offsets[row % 10]);
        assert_eq!(table.record(offsets[row])?.keys, &[(row % 10) as i64]);
        assert_eq!(table.record(offsets[row])?.payload, [0; 8]);
    }
    for (key, &offset) in offsets[..10].iter().enumerate() {
        table
            .payload_mut(offset)?
            .copy_from_slice(&(key as u64 * 7).to_ne_bytes());
    }
    for (row, &offset) in offsets.iter().enumerate() {
        let payload = table.record(offset)?.payload;
        assert_eq!(payload, ((row % 10) as u64 * 7).to_ne_bytes());
    }
    assert!(table.payload_mut(offsets[0] + 1).is_err());

    let more: Vec<i32> = (5..15).collect();
    let more_keys = [ColumnView::try_new(&more, None)?];
    let more_hashes: Vec<u32> = more.iter().map(|&value| hash_i32(value)).collect();
    let (more_offsets, created) = resolve_all(&mut table, &more_hashes, &more_keys[..])?;
    assert_eq!(
        created,
        (5..10).collect::<Vec<_>>(),
        "keys 10 to 14 are new"
    );
    assert_eq!(more_offsets[..5], offsets[5..10]);
    assert_eq!(table.stats().records, 15);
    let (found, matches) = probe_all(&table, &more_hashes, &more_keys[..])?;
    assert_eq!(found.len(), 10);
    assert_eq!(matches, more_offsets);
    Ok(())
}

#[test]
fn a_full_table_resolves_known_keys_only() -> Result<()> {
    let values: Vec<i32> = (0..20).collect();
    let keys = [ColumnView::try_new(&values, None)?];
    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
    let mut words = words_for(&ONE_INT4, 4)?;
    let mut table = TableMut::create_in(&mut words, &ONE_INT4, 4)?;
    let mut pending_words = all_rows(20);
    let mut pending = RowMask::try_new(20, &mut pending_words)?;
    let mut offsets = vec![0; 20];
    let mut inserted_words = [0];
    let mut inserted = RowMask::try_new(20, &mut inserted_words)?;
    let resolved = table.find_or_insert(
        &hashes,
        &keys[..],
        &mut pending,
        &mut offsets,
        &mut inserted,
    )?;
    assert_eq!(resolved, 4);
    assert_eq!(rows_of(&pending.as_view()), (4..20).collect::<Vec<_>>());
    assert_eq!(inserted_words, [0b1111]);

    let known: Vec<i32> = [3, 2, 1, 0, 3].into();
    let known_keys = [ColumnView::try_new(&known, None)?];
    let known_hashes: Vec<u32> = known.iter().map(|&value| hash_i32(value)).collect();
    let (known_offsets, created) = resolve_all(&mut table, &known_hashes, &known_keys[..])?;
    assert!(created.is_empty(), "no room is needed for known keys");
    assert_eq!(known_offsets[0], offsets[3]);
    assert_eq!(known_offsets[4], offsets[3]);
    assert_eq!(known_offsets[3], offsets[0]);
    assert_eq!(table.stats().records, 4);
    Ok(())
}

#[test]
fn a_walk_visits_every_record_once_in_insertion_order() -> Result<()> {
    let values: Vec<i32> = (0..200).map(|row| row * 7 % 50).collect();
    let keys = [ColumnView::try_new(&values, None)?];
    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
    let mut words = words_for(&ONE_INT4, 200)?;
    let table = TableMut::create_in(&mut words, &ONE_INT4, 200)?;
    assert!(scan_all(&table, 64)?.is_empty());
    let mut offsets = insert_all(
        &table,
        &hashes[..120],
        &[ColumnView::try_new(&values[..120], None)?][..],
        None,
    )?;
    offsets.extend(insert_all(
        &table,
        &hashes[120..],
        &[ColumnView::try_new(&values[120..], None)?][..],
        None,
    )?);
    let _ = keys;
    for step in [1, 7, 64, 500] {
        assert_eq!(scan_all(&table, step)?, offsets, "walks of {step}");
    }
    let mut cursor = Cursor::from_raw(HEADER as u64 + 4);
    assert!(
        table.scan(&mut cursor, &mut [0; 8]).is_err(),
        "a cursor inside a record"
    );
    let mut cursor = Cursor::from_raw(table.stats().bytes_used + 8);
    assert!(
        table.scan(&mut cursor, &mut [0; 8]).is_err(),
        "a cursor past the records"
    );
    let mut cursor = Cursor::start();
    assert_eq!(
        table.scan(&mut cursor, &mut [])?,
        0,
        "nowhere to put records"
    );
    assert_eq!(cursor, Cursor::start());
    Ok(())
}

#[test]
fn growing_keeps_records_and_their_offsets() -> Result<()> {
    let values: Vec<i32> = (0..100).map(|row| row % 30).collect();
    let keys = [ColumnView::try_new(&values, None)?];
    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
    let payload = payload_for(100);
    let mut words = words_for(&ONE_INT4, 16)?;
    let mut pending_words = all_rows(100);
    let mut offsets = vec![0; 100];
    let mut scans = Vec::new();
    for (capacity, expected) in [(16, 16), (64, 48), (512, 36), (5000, 0)] {
        let len = region_size(&ONE_INT4, capacity)?;
        words.resize(len / 8, 0);
        let table = if capacity == 16 {
            TableMut::create_in(&mut words, &ONE_INT4, 16)?
        } else {
            let mut table = TableMut::exclusive(&mut words)?;
            table.grow(len)?;
            assert_eq!(table.stats().region_len, len as u64);
            assert_eq!(
                table.stats().buckets,
                model_buckets(capacity),
                "growing into a region sized for {capacity} gives its buckets"
            );
            table
        };
        let mut pending = RowMask::try_new(100, &mut pending_words)?;
        let inserted = table.insert(
            &hashes,
            &keys[..],
            Some(&payload),
            &mut pending,
            &mut offsets,
        )?;
        assert_eq!(inserted, expected, "capacity {capacity}");
        scans.push(scan_all(&table, 64)?);
    }
    let mut table = TableMut::exclusive(&mut words)?;
    assert_eq!(table.stats().records, 100);
    assert_eq!(table.stats().buckets, 16384);
    assert_eq!(scans[3], offsets, "the walk is the insertion order");
    assert_eq!(scans[2], offsets);
    assert_eq!(scans[1], offsets[..64]);
    assert_eq!(scans[0], offsets[..16]);
    let (found, matches) = probe_all(&table, &hashes, &keys[..])?;
    assert_eq!(found.len(), 100);
    for row in 0..100 {
        let record = table.record(matches[row])?;
        assert_eq!(record.keys, &[(row % 30) as i64]);
        assert_eq!(record.payload, offsets_payload(&offsets, matches[row]));
        assert_eq!(
            table.record(offsets[row])?.payload,
            (row as u64 * 10).to_ne_bytes()
        );
    }
    let mut current = matches[..30].to_vec();
    let mut chain_lengths = vec![1; 30];
    let mut rows_words = all_rows(30);
    loop {
        let rows = RowMaskView::try_new(30, &rows_words)?;
        let mut more_words = [0];
        let mut more = RowMask::try_new(30, &mut more_words)?;
        table.next_match(&mut current, &rows, &mut more)?;
        let hits = rows_of(&more.as_view());
        if hits.is_empty() {
            break;
        }
        for key in hits {
            chain_lengths[key] += 1;
        }
        rows_words = vec![more_words[0]];
    }
    assert_eq!(
        chain_lengths,
        vec![4; 10]
            .into_iter()
            .chain(vec![3; 20])
            .collect::<Vec<_>>()
    );

    let len = table.stats().region_len as usize;
    assert!(table.grow(len - 8).is_err(), "no shrinking");
    assert!(table.grow(len + 4).is_err(), "a multiple of 8");
    assert!(table.grow(len + 8).is_err(), "not past the slice");
    table.grow(len)?;
    assert_eq!(table.stats().records, 100);
    Ok(())
}

/// The payload that was inserted with the record at `offset`.
fn offsets_payload(offsets: &[u32], offset: u32) -> [u8; 8] {
    let row = offsets.iter().position(|&o| o == offset).unwrap();
    (row as u64 * 10).to_ne_bytes()
}

#[test]
fn growing_a_table_whose_records_crowd_the_buckets_halves_them() -> Result<()> {
    let config = TableConfig {
        keys: &[KeyKind::Int32],
        payload_size: 4000,
    };
    let values: Vec<i32> = (0..3).collect();
    let keys = [ColumnView::try_new(&values, None)?];
    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
    let mut words = words_for(&config, 3)?;
    let len = words.len() * 8;
    insert_all(
        &TableMut::create_in(&mut words, &config, 3)?,
        &hashes,
        &keys[..],
        None,
    )?;
    words.resize(len / 8 + 1, 0);
    let mut table = TableMut::exclusive(&mut words)?;
    table.grow(len + 8)?;
    assert_eq!(table.stats().buckets, 1024, "the floor stays");
    let (found, _) = probe_all(&table, &hashes, &keys[..])?;
    assert_eq!(found.len(), 3);
    Ok(())
}

/// A xorshift generator for test values.
fn random(state: &mut u64) -> u64 {
    *state ^= *state >> 12;
    *state ^= *state << 25;
    *state ^= *state >> 27;
    state.wrapping_mul(0x2545_F491_4F6C_DD1D)
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

#[test]
fn whole_words_normalize_like_rows() -> Result<()> {
    let mut state = 0x1234_5678_9abc_def1;
    for nrows in [64, 65, 130, 200] {
        let values: Vec<i32> = (0..nrows).map(|_| random(&mut state) as i32).collect();
        let mut non_nulls = all_rows(nrows);
        for (index, word) in non_nulls.iter_mut().enumerate() {
            *word &= random(&mut state) | 1 << (index % 64);
        }
        let dense = ColumnView::try_new(&values, Some(RowMaskView::try_new(nrows, &non_nulls)?))?;
        let rows = RowsOnly(ColumnView::try_new(
            &values,
            Some(RowMaskView::try_new(nrows, &non_nulls)?),
        )?);
        for index in 0..nrows.div_ceil(64) {
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
            ] {
                let (mut from_block, mut from_rows) = ([7; 64], [7; 64]);
                let block_bits = normalize_word(&dense, index, selected, &mut from_block)?;
                let row_bits = normalize_word(&rows, index, selected, &mut from_rows)?;
                assert_eq!(block_bits, row_bits, "{nrows} rows, word {index}");
                for bit in (0..64).filter(|bit| selected >> bit & 1 != 0) {
                    assert_eq!(from_block[bit], from_rows[bit], "{nrows} rows, row {bit}");
                    if row_bits >> bit & 1 == 0 {
                        assert_eq!(from_block[bit], 0, "a NULL key is 0");
                    }
                }
            }
        }
        // Both paths build tables that answer alike.
        let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
        let mut first = words_for(&ONE_INT4, nrows as u64)?;
        let mut second = words_for(&ONE_INT4, nrows as u64)?;
        let offsets = insert_all(
            &TableMut::create_in(&mut first, &ONE_INT4, nrows as u64)?,
            &hashes,
            &[dense][..],
            None,
        )?;
        let other = insert_all(
            &TableMut::create_in(&mut second, &ONE_INT4, nrows as u64)?,
            &hashes,
            &[rows][..],
            None,
        )?;
        assert_eq!(offsets, other);
        assert_eq!(first, second, "the regions are byte for byte the same");
    }
    Ok(())
}
