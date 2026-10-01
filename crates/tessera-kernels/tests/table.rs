#![forbid(unsafe_code)]

use anyhow::Result;
use proptest::prelude::*;
use tessera_core::{ColumnReader, ColumnView, RowMask, RowMaskView};
use tessera_kernels::decimal::{
    self, Decimal, DecimalWord, Partial, Partials, Special, SumState, Term, Terms,
};
use tessera_kernels::int32::{self, NullKeys, hash_combine, murmurhash32};
use tessera_kernels::ops::ArithmeticError;
use tessera_kernels::table::{
    CHUNK_HEADER, Cursor, FORMAT_VERSION, Fold, KeyKind, KeySource, LocalTable, MAX_CHUNK_LEN,
    MAX_KEYS, Slot, SumSlot, Table, TableConfig, UNIT_BITS, bloom, index_size, normalize_word,
    record_bytes,
};
use tessera_testing::{edge, flags, integer, property, values, words};

/// Bytes of the header, as the format fixes it.
const HEADER: usize = 96;
/// Byte offsets of the header fields the tests corrupt.
const MAGIC: usize = 0;
const VERSION: usize = 8;
const HEADER_SIZE: usize = 12;
const REGION_LEN: usize = 16;
const BUCKETS_OFFSET: usize = 24;
const RESERVED_USED: usize = 32;
const NRECORDS: usize = 40;
const NBUCKETS: usize = 48;
const BUCKET_SHIFT: usize = 52;
const RECORD_SIZE: usize = 56;
const NKEYS: usize = 64;
const FLAGS: usize = 68;
const KINDS: usize = 72;

/// Chunks of the tests: small, so that most tables span several.
const CHUNK: usize = 4096;

const ONE_INT4: TableConfig<'static> = TableConfig {
    keys: &[KeyKind::Int32],
    payload_size: 8,
};

/// A named way to damage an index.
type Corruption<'a> = (&'a str, &'a dyn Fn(&mut [u64]));

/// An empty table for `capacity` records of `config` over chunks of
/// [`CHUNK`] bytes.
fn local(config: &TableConfig<'_>, capacity: u64) -> Result<LocalTable> {
    LocalTable::new(config, capacity, CHUNK)
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

/// The chunk and the byte in it of a record reference, as the format
/// packs them.
fn placement(reference: u32) -> (usize, usize) {
    (
        (reference >> UNIT_BITS) as usize,
        (reference & ((1 << UNIT_BITS) - 1)) as usize * 8,
    )
}

/// The reference of the record at `byte` of chunk `chunk`.
fn reference(chunk: usize, byte: usize) -> u32 {
    ((chunk as u32) << UNIT_BITS) | (byte / 8) as u32
}

#[test]
fn a_created_table_is_empty_and_attaches_again() -> Result<()> {
    let config = TableConfig {
        keys: &[KeyKind::Int32, KeyKind::Int64],
        payload_size: 12,
    };
    let table = local(&config, 300)?;
    let stats = {
        let table = table.table()?;
        let stats = table.stats();
        assert_eq!(stats.records, 0);
        assert_eq!(stats.buckets, 1024);
        assert_eq!(stats.bytes_used, HEADER as u64 + 1024 * 4);
        assert_eq!(stats.region_len, HEADER as u64 + 1024 * 4);
        assert_eq!(table.key_kinds(), config.keys);
        assert_eq!(table.payload_size(), 12);
        assert_eq!(table.record_size(), model_record_size(2, 12));
        stats
    };
    let again = table.table()?;
    assert_eq!(again.stats(), stats);
    assert_eq!(again.key_kinds(), config.keys);
    assert_eq!(table.chunks(), 0, "no chunk before the first record");
    Ok(())
}

#[test]
fn index_size_covers_header_and_buckets() -> Result<()> {
    let keys = [KeyKind::Int32; 3];
    for payload_size in [0, 1, 8, 13] {
        let config = TableConfig {
            keys: &keys,
            payload_size,
        };
        assert_eq!(record_bytes(&config)?, model_record_size(3, payload_size));
        for capacity in [0, 1, 511, 512, 513, 1000, 100_000] {
            let size = index_size(&config, capacity)?;
            assert_eq!(
                size,
                HEADER + model_buckets(capacity) as usize * 4,
                "{payload_size} payload, {capacity} records"
            );
            assert!(LocalTable::new(&config, capacity, CHUNK).is_ok());
        }
    }
    Ok(())
}

#[test]
fn configurations_are_checked() -> Result<()> {
    let no_keys = TableConfig {
        keys: &[],
        payload_size: 0,
    };
    assert!(index_size(&no_keys, 1).is_err());
    assert!(local(&no_keys, 1).is_err());
    let too_many = TableConfig {
        keys: &[KeyKind::Int64; MAX_KEYS + 1],
        payload_size: 0,
    };
    assert!(index_size(&too_many, 1).is_err());
    let most = TableConfig {
        keys: &[KeyKind::Int64; MAX_KEYS],
        payload_size: 0,
    };
    assert!(local(&most, 1).is_ok());
    let huge_payload = TableConfig {
        keys: &[KeyKind::Int32],
        payload_size: usize::MAX - 100,
    };
    assert!(index_size(&huge_payload, 1).is_err());
    let wide_payload = TableConfig {
        keys: &[KeyKind::Int32],
        payload_size: 1 << 33,
    };
    assert!(index_size(&wide_payload, 1).is_err());
    for bad in [0, 4, 12, MAX_CHUNK_LEN + 8] {
        assert!(
            LocalTable::new(&ONE_INT4, 1, bad).is_err(),
            "chunks of {bad} bytes"
        );
    }
    assert!(LocalTable::new(&ONE_INT4, 1, MAX_CHUNK_LEN).is_ok());
    Ok(())
}

#[test]
fn a_corrupt_header_is_refused() -> Result<()> {
    let config = TableConfig {
        keys: &[KeyKind::Int32, KeyKind::Int64],
        payload_size: 8,
    };
    let len = (HEADER + 1024 * 4) as u64;
    let corruptions: &[Corruption<'_>] = &[
        ("magic", &|w| set_u64(w, MAGIC, 0x5445_5353_5f54_4142)),
        ("version", &|w| set_u32(w, VERSION, FORMAT_VERSION + 1)),
        ("header size", &|w| set_u32(w, HEADER_SIZE, 88)),
        ("index longer than given", &|w| {
            set_u64(w, REGION_LEN, len + 8)
        }),
        ("index shorter than buckets", &|w| {
            set_u64(w, REGION_LEN, len - 8)
        }),
        ("index not a multiple of 8", &|w| {
            set_u64(w, REGION_LEN, len - 4)
        }),
        ("buckets moved", &|w| {
            set_u64(w, BUCKETS_OFFSET, HEADER as u64 + 8)
        }),
        ("buckets misaligned", &|w| {
            set_u64(w, BUCKETS_OFFSET, HEADER as u64 - 4)
        }),
        ("reserved word", &|w| set_u64(w, RESERVED_USED, 8)),
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
        let mut table = local(&config, 100)?;
        corrupt(table.index_words());
        assert!(table.table().is_err(), "{what} was accepted");
    }
    let mut table = local(&config, 100)?;
    set_bytes(table.index_words(), KINDS + 2, &[3]);
    assert!(
        table.table().is_ok(),
        "a kind byte past the keys is ignored"
    );
    set_u64(table.index_words(), NRECORDS, 12);
    assert!(
        table.table().is_ok(),
        "the record count is the links', not checked against chunks"
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

/// Append and link every row of a batch, expecting all of them to go in.
fn insert_all<K: KeySource + ?Sized>(
    table: &mut LocalTable,
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
    table: &Table<'_>,
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
    let mut table = local(&ONE_INT4, 200)?;
    let offsets = insert_all(&mut table, &hashes, &keys[..], Some(&payload))?;
    assert_eq!(
        table.chunks(),
        2,
        "a record of one key and eight payload bytes takes 32 bytes: 127 per chunk"
    );
    let table = table.table()?;
    assert_eq!(table.stats().records, 200);
    assert_eq!(table.stats().bytes_used, HEADER as u64 + 1024 * 4);
    let mut seen = offsets.clone();
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(seen.len(), 200, "every record has its own reference");
    assert_eq!(placement(offsets[126]), (0, CHUNK_HEADER + 126 * 32));
    assert_eq!(
        placement(offsets[127]),
        (1, CHUNK_HEADER),
        "the next chunk takes the rest"
    );
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
    let mut table = local(&ONE_INT4, 100)?;
    insert_all(&mut table, &hashes, &keys[..], Some(&payload))?;
    let table = table.table()?;

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

    let mut table = local(&ONE_INT4, 2)?;
    let mut offsets = [0; 2];
    table.insert(&hashes, &keys[..], None, &mut valid, &mut offsets)?;
    let table = table.table()?;
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
    assert_eq!(next, matches, "references without a next record stay");

    let mut rejected_words = [0b11];
    let mut rejected = RowMask::try_new(2, &mut rejected_words)?;
    int32::hash(
        &keys[0],
        &rows,
        NullKeys::Reject,
        &mut hashes,
        &mut rejected,
    )?;
    let mut table = local(&ONE_INT4, 2)?;
    table.insert(&hashes, &keys[..], None, &mut rejected, &mut offsets)?;
    let table = table.table()?;
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
    let mut table = local(&config, 100)?;
    let offsets = insert_all(&mut table, &hashes, &keys, None)?;
    let table = table.table()?;
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
fn a_full_chunk_leaves_the_rest_pending_until_linked_elsewhere() -> Result<()> {
    let values: Vec<i32> = (0..100).collect();
    let keys = [ColumnView::try_new(&values, None)?];
    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
    // A chunk of exactly sixteen records of 32 bytes after its used mark.
    let mut table = LocalTable::new(&ONE_INT4, 100, CHUNK_HEADER + 16 * 32)?;
    let chunk = table.add_chunk()?;
    let mut pending_words = all_rows(100);
    let mut offsets = vec![0; 100];
    {
        let shared = table.table()?;
        let mut pending = RowMask::try_new(100, &mut pending_words)?;
        let appended =
            shared.append(chunk, &hashes, &keys[..], None, &mut pending, &mut offsets)?;
        assert_eq!(appended, 16, "a chunk of sixteen records holds sixteen");
        assert_eq!(rows_of(&pending.as_view()), (16..100).collect::<Vec<_>>());
        assert_eq!(
            shared.append(chunk, &hashes, &keys[..], None, &mut pending, &mut offsets)?,
            0
        );
        assert_eq!(
            shared.stats().records,
            0,
            "appended records are not linked yet"
        );
        let (found, _) = probe_all(&shared, &hashes, &keys[..])?;
        assert!(found.is_empty(), "nor found");
        let mut from = CHUNK_HEADER;
        assert_eq!(shared.link(chunk, &mut from)?, 16);
        assert_eq!(from, CHUNK_HEADER + 16 * 32);
        assert_eq!(shared.link(chunk, &mut from)?, 0, "nothing is linked twice");
        assert_eq!(shared.stats().records, 16);
        let (found, matches) = probe_all(&shared, &hashes, &keys[..])?;
        assert_eq!(found, (0..16).collect::<Vec<_>>());
        assert_eq!(matches[..16], offsets[..16]);
    }
    assert_eq!(table.chunk_words(chunk)[0], (CHUNK_HEADER + 16 * 32) as u64);
    // The rest go into a second chunk and are found alongside.
    let second = table.add_chunk()?;
    let shared = table.table()?;
    let mut pending = RowMask::try_new(100, &mut pending_words)?;
    shared.append(second, &hashes, &keys[..], None, &mut pending, &mut offsets)?;
    let mut from = CHUNK_HEADER;
    shared.link(second, &mut from)?;
    assert_eq!(placement(offsets[16]), (second, CHUNK_HEADER));
    let (found, _) = probe_all(&shared, &hashes, &keys[..])?;
    assert_eq!(found, (0..32).collect::<Vec<_>>());
    // Appending to or linking a chunk that does not exist, or linking from
    // inside a record or past the used mark, fails.
    assert!(
        shared
            .append(9, &hashes, &keys[..], None, &mut pending, &mut offsets)
            .is_err()
    );
    let mut from = CHUNK_HEADER;
    assert!(shared.link(9, &mut from).is_err());
    let mut inside = CHUNK_HEADER + 4;
    assert!(shared.link(chunk, &mut inside).is_err());
    let mut past = CHUNK_HEADER + 17 * 32;
    assert!(shared.link(chunk, &mut past).is_err());
    Ok(())
}

#[test]
fn linking_counts_the_records_whose_keys_were_there_already() -> Result<()> {
    // Two participants' chunks: keys 0..40 with every key below 10 twice
    // in the first, and 30..60 in the second.
    let first: Vec<i32> = (0..40).chain(0..10).collect();
    let second: Vec<i32> = (30..60).collect();
    let mut table = LocalTable::new(&ONE_INT4, 100, 4096)?;
    let chunks = [table.add_chunk()?, table.add_chunk()?];
    let shared = table.table()?;
    for (chunk, values) in chunks.iter().zip([&first, &second]) {
        let keys = [ColumnView::try_new(values, None)?];
        let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
        let mut pending_words = all_rows(values.len());
        let mut pending = RowMask::try_new(values.len(), &mut pending_words)?;
        let mut offsets = vec![0; values.len()];
        shared.append(*chunk, &hashes, &keys[..], None, &mut pending, &mut offsets)?;
    }
    let mut from = [CHUNK_HEADER; 2];
    assert_eq!(shared.link_counting(chunks[0], &mut from[0])?, (50, 10));
    // Keys 30..40 were there from the first chunk; linking nothing more
    // counts nothing.
    assert_eq!(shared.link_counting(chunks[1], &mut from[1])?, (30, 10));
    assert_eq!(shared.link_counting(chunks[1], &mut from[1])?, (0, 0));
    assert_eq!(shared.stats().records, 80);
    Ok(())
}

#[test]
fn a_record_larger_than_a_chunk_is_refused() -> Result<()> {
    let config = TableConfig {
        keys: &[KeyKind::Int32],
        payload_size: 64,
    };
    let values = [1];
    let keys = [ColumnView::try_new(&values, None)?];
    let mut table = LocalTable::new(&config, 1, CHUNK_HEADER + 64)?;
    let mut pending_words = [1];
    let mut pending = RowMask::try_new(1, &mut pending_words)?;
    let error = table
        .insert(&[hash_i32(1)], &keys[..], None, &mut pending, &mut [0])
        .unwrap_err();
    assert!(error.to_string().contains("does not fit"), "{error}");
    Ok(())
}

#[test]
fn batches_of_every_shape_round_trip() -> Result<()> {
    for nrows in [0, 1, 63, 64, 65, 200, 1000] {
        let values: Vec<i32> = (0..nrows as i32).map(|row| row * 3 - 1000).collect();
        let keys = [ColumnView::try_new(&values, None)?];
        let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
        let payload = payload_for(nrows);
        let mut table = local(&ONE_INT4, nrows as u64)?;
        let offsets = insert_all(&mut table, &hashes, &keys[..], Some(&payload))?;
        let table = table.table()?;
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
    let mut table = local(&config, 1)?;
    let offsets = insert_all(&mut table, &hashes, &keys[..], None)?;
    assert_eq!(table.table()?.record(offsets[0])?.payload, [0; 13]);
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
    let mut table = local(&config, 70)?;
    let chunk = table.add_chunk()?;
    let mut pending_words = all_rows(70);
    let mut offsets = vec![0; 70];
    let rows_words = all_rows(70);
    let rows = RowMaskView::try_new(70, &rows_words)?;
    {
        let shared = table.table()?;
        let mut pending = RowMask::try_new(70, &mut pending_words)?;
        assert!(
            shared
                .append(
                    chunk,
                    &hashes,
                    &one_key[..],
                    None,
                    &mut pending,
                    &mut offsets
                )
                .is_err()
        );
        assert!(
            shared
                .append(
                    chunk,
                    &hashes[..69],
                    &two_keys[..],
                    None,
                    &mut pending,
                    &mut offsets
                )
                .is_err()
        );
        assert!(
            shared
                .append(
                    chunk,
                    &hashes,
                    &two_keys[..],
                    None,
                    &mut pending,
                    &mut offsets[..69]
                )
                .is_err()
        );
        assert!(
            shared
                .append(
                    chunk,
                    &hashes,
                    &two_keys[..],
                    Some(&payload[..8]),
                    &mut pending,
                    &mut offsets
                )
                .is_err()
        );
        assert_eq!(pending.as_view().selected_count(), 70);
        assert_eq!(shared.stats().records, 0);
        let mut found_words = vec![0; 2];
        let mut found = RowMask::try_new(70, &mut found_words)?;
        let mut matches = vec![0; 70];
        assert!(
            shared
                .probe(&hashes, &one_key[..], &rows, &mut matches, &mut found)
                .is_err()
        );
        assert!(
            shared
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
            shared
                .probe(&hashes, &two_keys[..], &rows, &mut matches, &mut short)
                .is_err()
        );
        assert!(
            shared
                .next_match(&mut offsets[..69], &rows, &mut found)
                .is_err()
        );
        assert!(shared.next_match(&mut offsets, &rows, &mut short).is_err());
    }
    assert_eq!(
        table.chunk_words(chunk)[0],
        CHUNK_HEADER as u64,
        "the chunk took nothing"
    );
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

/// Probe the first row of a batch through a fresh attachment.
fn table_probe_one(
    table: &LocalTable,
    hashes: &[u32],
    keys: &[ColumnView<'_, i32>],
) -> Result<bool> {
    let nrows = hashes.len();
    let table = table.table()?;
    let rows_words = [1];
    let rows = RowMaskView::try_new(nrows, &rows_words)?;
    let mut found_words = [0];
    let mut found = RowMask::try_new(nrows, &mut found_words)?;
    let mut matches = vec![0; nrows];
    table.probe(hashes, keys, &rows, &mut matches, &mut found)?;
    Ok(found_words[0] == 1)
}

#[test]
fn a_corrupt_reference_chain_or_used_mark_is_an_error() -> Result<()> {
    let values: Vec<i32> = (0..5).collect();
    let keys = [ColumnView::try_new(&values, None)?];
    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
    let mut table = local(&ONE_INT4, 5)?;
    let offsets = insert_all(&mut table, &hashes, &keys[..], None)?;
    assert!(table_probe_one(&table, &hashes, &keys)?);
    let bucket = HEADER + (hashes[0] >> 22) as usize * 4;
    let head = table.index_words()[bucket / 8];
    for (what, damaged) in [
        ("a chunk that does not exist", reference(3, CHUNK_HEADER)),
        ("a chunk's used mark", reference(0, 0) | 1 << UNIT_BITS),
        ("past the chunk", reference(0, CHUNK - 8)),
        ("inside a record", offsets[0] + 1),
    ] {
        set_u32(table.index_words(), bucket, damaged);
        assert!(
            table_probe_one(&table, &hashes, &keys).is_err(),
            "a head at {what}"
        );
        table.index_words()[bucket / 8] = head;
    }
    assert!(table_probe_one(&table, &hashes, &keys)?);
    assert!(
        table.table()?.record(offsets[0] + 1).is_err(),
        "a reference inside a record"
    );

    // The record of key 0 chains to itself: a probe with its hash and an
    // absent key walks the cycle and gives up.
    let (chunk, byte) = placement(offsets[0]);
    set_u32(table.chunk_words(chunk), byte + 4, offsets[0]);
    let absent = [999; 5];
    let absent_keys = [ColumnView::try_new(&absent, None)?];
    let error = table_probe_one(&table, &hashes, &absent_keys).unwrap_err();
    assert!(error.to_string().contains("longer"), "{error}");

    // A used mark that ends no record, or lies past the chunk.
    for used in [12, CHUNK as u64 + 8] {
        table.chunk_words(0)[0] = used;
        assert!(
            table
                .table_mut()?
                .scan(&mut Cursor::start(), &mut [0; 8])
                .is_err()
        );
        let mut from = CHUNK_HEADER;
        assert!(table.table()?.link(0, &mut from).is_err());
    }
    Ok(())
}

/// Resolve every row of a batch to a record, adding chunks and building a
/// larger index as needed: the references and the rows whose record was
/// created.
fn resolve_all<K: KeySource + ?Sized>(
    table: &mut LocalTable,
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

/// Every record of a table in the order it was appended, through walks of
/// `step`.
fn scan_all(table: &mut LocalTable, step: usize) -> Result<Vec<u32>> {
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

#[test]
fn rows_find_or_create_the_record_of_their_keys() -> Result<()> {
    let values: Vec<i32> = (0..100).map(|row| row % 10).collect();
    let keys = [ColumnView::try_new(&values, None)?];
    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
    let mut table = local(&ONE_INT4, 20)?;
    let (offsets, created) = resolve_all(&mut table, &hashes, &keys[..])?;
    assert_eq!(
        created,
        (0..10).collect::<Vec<_>>(),
        "the first row of each key"
    );
    assert_eq!(table.table()?.stats().records, 10);
    for row in 0..100 {
        assert_eq!(offsets[row], offsets[row % 10]);
        assert_eq!(
            table.table()?.record(offsets[row])?.keys,
            &[(row % 10) as i64]
        );
        assert_eq!(table.table()?.record(offsets[row])?.payload, [0; 8]);
    }
    {
        let mut writer = table.table_mut()?;
        for (key, &offset) in offsets[..10].iter().enumerate() {
            writer
                .payload_mut(offset)?
                .copy_from_slice(&(key as u64 * 7).to_ne_bytes());
        }
        assert!(writer.payload_mut(offsets[0] + 1).is_err());
    }
    for (row, &offset) in offsets.iter().enumerate() {
        let payload = table.table()?.record(offset)?.payload.to_vec();
        assert_eq!(payload, ((row % 10) as u64 * 7).to_ne_bytes());
    }

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
    let table = table.table()?;
    assert_eq!(table.stats().records, 15);
    let (found, matches) = probe_all(&table, &more_hashes, &more_keys[..])?;
    assert_eq!(found.len(), 10);
    assert_eq!(matches, more_offsets);
    Ok(())
}

#[test]
fn a_full_chunk_resolves_known_keys_only() -> Result<()> {
    let values: Vec<i32> = (0..20).collect();
    let keys = [ColumnView::try_new(&values, None)?];
    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
    // A chunk of four records.
    let mut table = LocalTable::new(&ONE_INT4, 20, CHUNK_HEADER + 4 * 32)?;
    let chunk = table.add_chunk()?;
    let mut pending_words = all_rows(20);
    let mut offsets = vec![0; 20];
    let mut inserted_words = [0];
    {
        let mut writer = table.table_mut()?;
        let mut pending = RowMask::try_new(20, &mut pending_words)?;
        let mut inserted = RowMask::try_new(20, &mut inserted_words)?;
        let resolved = writer.find_or_insert(
            chunk,
            &hashes,
            &keys[..],
            &mut pending,
            &mut offsets,
            &mut inserted,
        )?;
        assert_eq!(resolved, 4);
        assert_eq!(rows_of(&pending.as_view()), (4..20).collect::<Vec<_>>());
    }
    assert_eq!(inserted_words, [0b1111]);

    let known: Vec<i32> = [3, 2, 1, 0, 3].into();
    let known_keys = [ColumnView::try_new(&known, None)?];
    let known_hashes: Vec<u32> = known.iter().map(|&value| hash_i32(value)).collect();
    let mut known_pending = all_rows(5);
    let mut known_offsets = vec![0; 5];
    let mut created = [0];
    let resolved = table.table_mut()?.find_or_insert(
        chunk,
        &known_hashes,
        &known_keys[..],
        &mut RowMask::try_new(5, &mut known_pending)?,
        &mut known_offsets,
        &mut RowMask::try_new(5, &mut created)?,
    )?;
    assert_eq!(resolved, 5, "no room is needed for known keys");
    assert_eq!(created, [0]);
    assert_eq!(known_offsets[0], offsets[3]);
    assert_eq!(known_offsets[4], offsets[3]);
    assert_eq!(known_offsets[3], offsets[0]);
    assert_eq!(table.table()?.stats().records, 4);
    Ok(())
}

#[test]
fn an_index_takes_records_up_to_half_its_buckets_and_regrows() -> Result<()> {
    let values: Vec<i32> = (0..1500).collect();
    let keys = [ColumnView::try_new(&values, None)?];
    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
    let mut table = LocalTable::new(&ONE_INT4, 1, MAX_CHUNK_LEN)?;
    let chunk = table.add_chunk()?;
    let mut pending_words = all_rows(1500);
    let mut offsets = vec![0; 1500];
    let mut inserted_words = all_rows(1500);
    let resolved = table.table_mut()?.find_or_insert(
        chunk,
        &hashes,
        &keys[..],
        &mut RowMask::try_new(1500, &mut pending_words)?,
        &mut offsets,
        &mut RowMask::try_new(1500, &mut inserted_words)?,
    )?;
    assert_eq!(resolved, 512, "1024 buckets take 512 records");
    // The owner builds larger indexes as it goes; references stay.
    let first = offsets[..512].to_vec();
    let (all, created) = resolve_all(&mut table, &hashes, &keys[..])?;
    assert_eq!(all[..512], first);
    assert_eq!(created, (512..1500).collect::<Vec<_>>());
    assert_eq!(table.chunks(), 1, "the records never moved");
    let shared = table.table()?;
    assert_eq!(shared.stats().records, 1500);
    assert!(shared.stats().buckets >= 4096);
    let (found, matches) = probe_all(&shared, &hashes, &keys[..])?;
    assert_eq!(found.len(), 1500);
    assert_eq!(matches, all);
    Ok(())
}

#[test]
fn a_walk_visits_every_record_once_in_appended_order() -> Result<()> {
    let values: Vec<i32> = (0..400).map(|row| row * 7 % 50).collect();
    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
    let mut table = local(&ONE_INT4, 400)?;
    assert!(scan_all(&mut table, 64)?.is_empty());
    let mut offsets = insert_all(
        &mut table,
        &hashes[..120],
        &[ColumnView::try_new(&values[..120], None)?][..],
        None,
    )?;
    offsets.extend(insert_all(
        &mut table,
        &hashes[120..],
        &[ColumnView::try_new(&values[120..], None)?][..],
        None,
    )?);
    assert_eq!(table.chunks(), 4);
    for step in [1, 7, 64, 500] {
        assert_eq!(scan_all(&mut table, step)?, offsets, "walks of {step}");
    }
    let writer = table.table_mut()?;
    for (what, raw) in [
        ("inside a record", CHUNK_HEADER as u64 + 4),
        ("before the first record", 0),
        ("past the chunks", (6u64 << 32) | CHUNK_HEADER as u64),
    ] {
        let mut cursor = Cursor::from_raw(raw);
        assert!(
            writer.scan(&mut cursor, &mut [0; 8]).is_err(),
            "a cursor {what}"
        );
    }
    let mut cursor = Cursor::start();
    assert_eq!(
        writer.scan(&mut cursor, &mut [])?,
        0,
        "nowhere to put records"
    );
    assert_eq!(cursor, Cursor::start());
    Ok(())
}

#[test]
fn regrowing_keeps_records_and_their_references() -> Result<()> {
    let values: Vec<i32> = (0..100).map(|row| row % 30).collect();
    let keys = [ColumnView::try_new(&values, None)?];
    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
    let payload = payload_for(100);
    let mut table = local(&ONE_INT4, 16)?;
    let offsets = insert_all(&mut table, &hashes, &keys[..], Some(&payload))?;
    assert_eq!(table.table()?.stats().buckets, 1024);
    let before = scan_all(&mut table, 64)?;
    table.regrow(5000)?;
    assert_eq!(table.table()?.stats().buckets, model_buckets(5000));
    assert_eq!(
        table.table()?.stats().region_len,
        (HEADER as u64) + model_buckets(5000) * 4
    );
    assert_eq!(
        scan_all(&mut table, 64)?,
        before,
        "the records stay where they were"
    );
    assert_eq!(before, offsets);
    let shared = table.table()?;
    assert_eq!(shared.stats().records, 100);
    let (found, matches) = probe_all(&shared, &hashes, &keys[..])?;
    assert_eq!(found.len(), 100);
    for row in 0..100 {
        let record = shared.record(matches[row])?;
        assert_eq!(record.keys, &[(row % 30) as i64]);
        assert_eq!(
            shared.record(offsets[row])?.payload,
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
        shared.next_match(&mut current, &rows, &mut more)?;
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
    assert!(table.regrow(16).is_err(), "no shrinking");
    assert_eq!(table.table()?.stats().records, 100);
    Ok(())
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
        // Both paths build tables that answer alike, byte for byte.
        let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
        let mut first = local(&ONE_INT4, nrows as u64)?;
        let mut second = local(&ONE_INT4, nrows as u64)?;
        let offsets = insert_all(&mut first, &hashes, &[dense][..], None)?;
        let other = insert_all(&mut second, &hashes, &[rows][..], None)?;
        assert_eq!(offsets, other);
        assert_eq!(first.index_words(), second.index_words());
        for chunk in 0..first.chunks() {
            assert_eq!(first.chunk_words(chunk), second.chunk_words(chunk));
        }
    }
    Ok(())
}

/// Probe `rows` of a batch in one call: the rows found and their matches.
fn probe_rows(
    table: &Table<'_>,
    hashes: &[u32],
    keys: &[ColumnView<'_, i32>],
    rows: &[u64],
) -> Result<(Vec<u64>, Vec<u32>)> {
    let nrows = hashes.len();
    let view = RowMaskView::try_new(nrows, rows)?;
    let mut found_words = vec![0; nrows.div_ceil(64)];
    let mut found = RowMask::try_new(nrows, &mut found_words)?;
    let mut matches = vec![0; nrows];
    table.probe(hashes, keys, &view, &mut matches, &mut found)?;
    Ok((found_words, matches))
}

#[test]
fn a_word_probed_at_once_answers_as_rows_probed_alone() -> Result<()> {
    // Three thousand records over 8192 buckets, and over many chunks, with
    // equal keys and equal hashes of different rows in chains.
    let values: Vec<i32> = (0..3000).map(|row| row % 1700).collect();
    let keys = [ColumnView::try_new(&values, None)?];
    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
    let mut table = local(&ONE_INT4, 3000)?;
    insert_all(&mut table, &hashes, &keys[..], None)?;
    let table = table.table()?;
    // Present keys, absent ones past them, and edges.
    let probes = proptest::collection::vec(prop_oneof![0..2000, edge::<i32>()], 0..=200);
    property(probes, |probe_values| -> Result<()> {
        let nrows = probe_values.len();
        let probe_keys = [ColumnView::try_new(&probe_values, None)?];
        let probe_hashes: Vec<u32> = probe_values.iter().map(|&value| hash_i32(value)).collect();
        let all = all_rows(nrows);
        let (found, matches) = probe_rows(&table, &probe_hashes, &probe_keys, &all)?;
        for row in 0..nrows {
            let mut one = vec![0; all.len()];
            one[row / 64] = 1 << (row % 64);
            let (alone, alone_matches) = probe_rows(&table, &probe_hashes, &probe_keys, &one)?;
            let hit = alone[row / 64] >> (row % 64) & 1;
            assert_eq!(found[row / 64] >> (row % 64) & 1, hit, "row {row}");
            assert_eq!(
                hit == 1,
                (0..1700).contains(&probe_values[row]),
                "row {row}"
            );
            if hit == 1 {
                assert_eq!(matches[row], alone_matches[row], "row {row}");
            }
        }
        Ok(())
    });
    Ok(())
}

#[test]
fn a_word_probed_at_once_detects_a_cycle() -> Result<()> {
    let values: Vec<i32> = (0..64).collect();
    let keys = [ColumnView::try_new(&values, None)?];
    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
    let mut table = local(&ONE_INT4, 64)?;
    let offsets = insert_all(&mut table, &hashes, &keys[..], None)?;
    // The record of key 0 chains to itself; absent keys with the same
    // hashes walk the cycle in the whole-word path.
    let (chunk, byte) = placement(offsets[0]);
    set_u32(table.chunk_words(chunk), byte + 4, offsets[0]);
    let absent = [999; 64];
    let absent_keys = [ColumnView::try_new(&absent, None)?];
    let error = probe_rows(&table.table()?, &hashes, &absent_keys, &all_rows(64)).unwrap_err();
    assert!(error.to_string().contains("longer"), "{error}");
    Ok(())
}

#[test]
fn gather_reads_one_payload_word_of_each_selected_match() -> Result<()> {
    let config = TableConfig {
        keys: &[KeyKind::Int32],
        payload_size: 16,
    };
    let nrows = 100;
    let values: Vec<i32> = (0..nrows as i32).collect();
    let keys = [ColumnView::try_new(&values, None)?];
    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
    // Payload words: the row times ten, then the row plus a million.
    let payload: Vec<u8> = (0..nrows as u64)
        .flat_map(|row| [row * 10, row + 1_000_000])
        .flat_map(u64::to_ne_bytes)
        .collect();
    let mut table = local(&config, nrows as u64)?;
    insert_all(&mut table, &hashes, &keys[..], Some(&payload))?;
    let table = table.table()?;
    let (_, matches) = probe_all(&table, &hashes, &keys[..])?;

    // Every third row, across both words of the batch.
    let selected: Vec<u64> = (0..2)
        .map(|index| {
            (0..64)
                .filter(|bit| (index * 64 + bit) % 3 == 0 && index * 64 + bit < nrows)
                .fold(0, |word, bit| word | 1 << bit)
        })
        .collect();
    let rows = RowMaskView::try_new(nrows, &selected)?;
    for (at, expected) in [
        (0, &(|row: u64| row * 10) as &dyn Fn(u64) -> u64),
        (8, &|row: u64| row + 1_000_000),
    ] {
        let mut out = vec![u64::MAX; nrows];
        table.gather(&matches, &rows, at, &mut out)?;
        for (row, &value) in out.iter().enumerate() {
            let wanted = if row % 3 == 0 {
                expected(row as u64)
            } else {
                u64::MAX
            };
            assert_eq!(value, wanted, "word at {at}, row {row}");
        }
    }

    // A word past the payload, short buffers and a bad reference are errors.
    let mut out = vec![0; nrows];
    assert!(table.gather(&matches, &rows, 9, &mut out).is_err());
    assert!(table.gather(&matches, &rows, usize::MAX, &mut out).is_err());
    assert!(table.gather(&matches[1..], &rows, 0, &mut out).is_err());
    assert!(table.gather(&matches, &rows, 0, &mut out[1..]).is_err());
    let mut inside = matches.clone();
    inside[0] += 1;
    assert!(table.gather(&inside, &rows, 0, &mut out).is_err());
    Ok(())
}

/// Append rows and link them grouped: the count of rows whose keys the
/// table held already.
fn insert_grouped_rows(
    table: &mut LocalTable,
    hashes: &[u32],
    keys: &[ColumnView<'_, i32>],
    payload: &[u8],
    rows: &[usize],
    offsets: &mut [u32],
) -> Result<usize> {
    let nrows = hashes.len();
    let mut pending_words = vec![0; nrows.div_ceil(64)];
    for &row in rows {
        pending_words[row / 64] |= 1 << (row % 64);
    }
    let mut pending = RowMask::try_new(nrows, &mut pending_words)?;
    let (inserted, duplicates) =
        table.insert_grouped(hashes, keys, Some(payload), &mut pending, offsets)?;
    assert_eq!(inserted, rows.len());
    assert_eq!(pending.as_view().selected_count(), 0);
    Ok(duplicates)
}

#[test]
fn grouped_records_of_a_key_lie_together() -> Result<()> {
    // Ten keys, ten rows each, all in one bucket: one hash for every row,
    // so each key's group lies among the other keys' records.
    let values: Vec<i32> = (0..100).map(|row| (row * 7) % 10).collect();
    let keys = [ColumnView::try_new(&values, None)?];
    let hashes = vec![0x1234_5678; 100];
    let payload = payload_for(100);
    let mut table = local(&ONE_INT4, 100)?;
    let mut offsets = vec![0; 100];
    let first: Vec<usize> = (0..30).collect();
    let second: Vec<usize> = (30..100).collect();
    let duplicates =
        insert_grouped_rows(&mut table, &hashes, &keys, &payload, &first, &mut offsets)?;
    assert_eq!(duplicates, 20, "a key's first row is new");
    let duplicates =
        insert_grouped_rows(&mut table, &hashes, &keys, &payload, &second, &mut offsets)?;
    assert_eq!(duplicates, 70, "every key was there already");

    // From the probe's record, the group gives every row of the key, one
    // step each, and ends where the next key begins.
    let table = table.table()?;
    let (found, matches) = probe_all(&table, &hashes, &keys[..])?;
    assert_eq!(found.len(), 100);
    let mut seen = [0; 10];
    for key in 0..10 {
        let mut offset = matches[key];
        loop {
            let record = table.record(offset)?;
            let row = u64::from_ne_bytes(record.payload.try_into().unwrap()) / 10;
            assert_eq!(values[row as usize], values[key]);
            seen[values[key] as usize] += 1;
            let mut offsets = [offset];
            let rows = [1];
            let mut next_words = [0];
            let mut next = RowMask::try_new(1, &mut next_words)?;
            table.next_in_group(&mut offsets, &RowMaskView::try_new(1, &rows)?, &mut next)?;
            if next_words[0] == 0 {
                break;
            }
            offset = offsets[0];
        }
    }
    assert_eq!(seen, [10; 10], "each group holds the ten rows of its key");
    Ok(())
}

#[test]
fn grouped_chains_span_chunks_and_survive_a_larger_index() -> Result<()> {
    let values: Vec<i32> = (0..400).map(|row| row % 7).collect();
    let keys = [ColumnView::try_new(&values, None)?];
    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
    let payload = payload_for(400);
    let mut table = local(&ONE_INT4, 64)?;
    let mut offsets = vec![0; 400];
    let rows: Vec<usize> = (0..400).collect();
    let duplicates =
        insert_grouped_rows(&mut table, &hashes, &keys, &payload, &rows, &mut offsets)?;
    assert_eq!(duplicates, 393, "the first row of each key is new");
    assert!(table.chunks() > 3);
    table.regrow(1000)?;

    // next_match walks the chain and finds the same next records, and
    // each key has as many as rows.
    let table = table.table()?;
    let (_, matches) = probe_all(
        &table,
        &hashes[..7],
        &[ColumnView::try_new(&values[..7], None)?][..],
    )?;
    for (key, &first) in matches.iter().enumerate() {
        let mut by_group = vec![first];
        let mut by_chain = vec![first];
        let mut records = 1;
        loop {
            let rows = [1];
            let view = RowMaskView::try_new(1, &rows)?;
            let mut group_words = [0];
            let mut chain_words = [0];
            table.next_in_group(
                &mut by_group,
                &view,
                &mut RowMask::try_new(1, &mut group_words)?,
            )?;
            table.next_match(
                &mut by_chain,
                &view,
                &mut RowMask::try_new(1, &mut chain_words)?,
            )?;
            assert_eq!(
                (group_words, by_group[0]),
                (chain_words, by_chain[0]),
                "key {key}"
            );
            if group_words[0] == 0 {
                break;
            }
            records += 1;
        }
        let rows = values.iter().filter(|&&value| value == values[key]).count();
        assert_eq!(records, rows, "key {key}");
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
        keys: &[KeyKind::Int32],
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
    let mut table = LocalTable::new(&config, 64, CHUNK_HEADER + 5 * 64)?;
    let (offsets, _) = resolve_all(&mut table, &hashes, &key_column[..])?;
    assert!(table.chunks() > 1, "the groups span chunks");
    // Two batches over the same rows: the first and the second half.
    for half in 0..2 {
        let selected: Vec<bool> = (0..nrows)
            .map(|row| (row < nrows / 2) == (half == 0))
            .collect();
        let selection = words_of(&selected);
        let rows = RowMaskView::try_new(nrows, &selection)?;
        let mut writer = table.table_mut()?;
        writer.count_rows(&offsets, &rows, 8)?;
        writer.count_values(&offsets, &rows, &x_column, 16)?;
        let slot = |value_at, flag_bit| Slot {
            value_at,
            flags_at: FLAGS_AT,
            flag_bit,
        };
        writer.fold(&offsets, &rows, &x_column, Fold::Sum, slot(24, 0))?;
        writer.fold(&offsets, &rows, &x_column, Fold::Min, slot(32, 1))?;
        writer.fold(&offsets, &rows, &y_column, Fold::Max, slot(40, 2))?;
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
    table.gather_key(&groups, &rows, 0, &mut key_values, &mut key_nulls)?;
    let mut fields = [(); 6].map(|_| vec![0u64; groups.len()]);
    for (index, field) in fields.iter_mut().enumerate() {
        table.gather(&groups, &rows, index * 8, field)?;
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
            keys: &[KeyKind::Int32],
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
        let mut table = LocalTable::new(&config, 256, CHUNK_HEADER + 64 * 128)?;
        let (offsets, _) = resolve_all(&mut table, &hashes, &key_column[..])?;
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
                table.table_mut()?.sum_terms(&offsets, &rows, &mut slots)?;
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
        table.gather_key(&groups, &rows, 0, &mut key_values, &mut key_nulls)?;
        for sum in 0..SUMS {
            let mut fields = [(); SumState::WORDS].map(|_| vec![0u64; groups.len()]);
            for (index, field) in fields.iter_mut().enumerate() {
                table.gather(&groups, &rows, at(sum) + index * 8, field)?;
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
            keys: &[KeyKind::Int32],
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
        let mut table = LocalTable::new(&config, 512, CHUNK_HEADER + 64 * 128)?;
        let (offsets, _) = resolve_all(&mut table, &hashes, &key_column[..])?;
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
            table
                .table_mut()?
                .sum_partials(&offsets, &rows, &mut slots)?;
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
        table.gather_key(&groups, &rows, 0, &mut key_values, &mut key_nulls)?;
        let mut matched_whole = 0;
        for sum in 0..SUMS {
            let mut fields = [(); SumState::WORDS].map(|_| vec![0u64; groups.len()]);
            for (index, field) in fields.iter_mut().enumerate() {
                table.gather(&groups, &rows, at(sum) + index * 8, field)?;
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
        keys: &[KeyKind::Int64],
        payload_size: 16,
    };
    let keys = [ColumnView::try_new(&[-5i64, -5, 1 << 40], None)?];
    let hashes: Vec<u32> = [-5i64, -5, 1 << 40].map(hash_i64).into();
    let mut table = local(&config, 8)?;
    let (offsets, created) = resolve_all(&mut table, &hashes, &keys[..])?;
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
    let error = writer
        .fold(&offsets, &rows, &big, Fold::Sum, slot)
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<ArithmeticError>().copied(),
        Some(ArithmeticError::BigintOutOfRange)
    );
    // The int8 keys come back whole, a negative one too.
    let mut values = vec![0; 3];
    let mut nulls = vec![true; 3];
    writer.gather_key(&offsets, &rows, 0, &mut values, &mut nulls)?;
    assert_eq!(values, [(-5i64) as u64, (-5i64) as u64, 1 << 40]);
    assert_eq!(nulls, [false; 3]);
    // Words past the payload, a flag past a word, a key past the keys.
    assert!(writer.count_rows(&offsets, &rows, 16).is_err());
    assert!(writer.count_rows(&offsets, &rows, 3).is_err());
    let past = Slot {
        flag_bit: 64,
        ..slot
    };
    assert!(writer.fold(&offsets, &rows, &big, Fold::Min, past).is_err());
    assert!(
        writer
            .gather_key(&offsets, &rows, 1, &mut values, &mut nulls)
            .is_err()
    );
    assert!(writer.count_rows(&offsets[..2], &rows, 8).is_err());
    Ok(())
}

/// A table of `count` int4 keys 0, 3, 6, ... and a filter of its records.
fn filtered_table(count: usize) -> Result<(LocalTable, Vec<u64>)> {
    let keys: Vec<i32> = (0..count as i32).map(|key| key * 3).collect();
    let hashes: Vec<u32> = keys.iter().map(|&key| hash_i32(key)).collect();
    let mut table = local(&ONE_INT4, count.max(1) as u64)?;
    let mut filter = vec![0; bloom::words_for(count as u64)?];
    if count > 0 {
        let column = [ColumnView::try_new(&keys, None)?];
        resolve_all(&mut table, &hashes, &column[..])?;
    }
    table.table()?.bloom(&mut filter)?;
    Ok((table, filter))
}

fn probe_filter(filter: &[u64], hashes: &[u32]) -> Result<usize> {
    let rows_words = all_rows(hashes.len());
    let rows = RowMaskView::try_new(hashes.len(), &rows_words)?;
    let mut found_words = vec![0; hashes.len().div_ceil(64)];
    bloom::probe(
        filter,
        hashes,
        &rows,
        &mut RowMask::try_new(hashes.len(), &mut found_words)?,
    )?;
    Ok(found_words
        .iter()
        .map(|word| word.count_ones() as usize)
        .sum())
}

#[test]
fn a_filter_lets_every_key_through_and_few_others() -> Result<()> {
    let count = 5000;
    let (table, filter) = filtered_table(count)?;
    assert!(table.chunks() > 1, "the records span chunks");
    assert_eq!(filter.len(), (count * 16).div_ceil(64).next_power_of_two());
    let present: Vec<u32> = (0..count as i32).map(|key| hash_i32(key * 3)).collect();
    assert_eq!(
        probe_filter(&filter, &present)?,
        count,
        "no key of the table is rejected"
    );
    // Keys 1, 4, 7, ... are absent: about one in a hundred gets through.
    let absent: Vec<u32> = (0..100_000).map(|key| hash_i32(key * 3 + 1)).collect();
    let through = probe_filter(&filter, &absent)?;
    assert!(through < 3000, "{through} of 100000 absent keys passed");
    Ok(())
}

#[test]
fn an_empty_table_rejects_everything_and_sizes_are_checked() -> Result<()> {
    let (_, filter) = filtered_table(0)?;
    assert_eq!(filter.len(), 1);
    let hashes: Vec<u32> = (0..200).map(hash_i32).collect();
    assert_eq!(probe_filter(&filter, &hashes)?, 0);
    // Not a power of two, and hashes of another row count.
    assert!(probe_filter(&[0; 3], &hashes).is_err());
    let rows_words = all_rows(4);
    let rows = RowMaskView::try_new(4, &rows_words)?;
    let mut found = [0];
    assert!(
        bloom::probe(
            &filter,
            &[1, 2],
            &rows,
            &mut RowMask::try_new(4, &mut found)?
        )
        .is_err()
    );
    assert!(bloom::words_for(u64::MAX).is_err());
    Ok(())
}

#[test]
fn a_null_group_key_and_int8_keys_pass_their_filter() -> Result<()> {
    // Keys 1, NULL, 2^40 under the grouping policy: the NULL's hash is the
    // policy's constant.
    let config = TableConfig {
        keys: &[KeyKind::Int64],
        payload_size: 8,
    };
    let values = [1_i64, 0, 1 << 40];
    let non_null = [0b101_u64];
    let column = [ColumnView::try_new(
        &values,
        Some(RowMaskView::try_new(3, &non_null)?),
    )?];
    let mut hashes = vec![0; 3];
    let all = all_rows(3);
    let mut valid = all.clone();
    tessera_kernels::int64::hash(
        &column[0],
        &RowMaskView::try_new(3, &all)?,
        NullKeys::Group,
        &mut hashes,
        &mut RowMask::try_new(3, &mut valid)?,
    )?;
    let mut table = local(&config, 4)?;
    let mut filter = vec![0; bloom::words_for(3)?];
    resolve_all(&mut table, &hashes, &column[..])?;
    table.table()?.bloom(&mut filter)?;
    assert_eq!(probe_filter(&filter, &hashes)?, 3);
    Ok(())
}

/// Records appended from payload columns hold what records appended from
/// the same payload laid out row by row hold: a word of the row's NULL
/// bits per 64 columns, then a word per column, 0 for a NULL; the rows
/// left out of the mask get none, and a column of the wrong length or a
/// payload of the wrong size is refused.
#[test]
fn payload_columns_append_the_records_a_payload_array_does() -> Result<()> {
    payload_columns_of(3)?;
    payload_columns_of(64)?;
    payload_columns_of(130)
}

fn payload_columns_of(ncolumns: usize) -> Result<()> {
    use tessera_kernels::table::{PayloadColumns, payload_null_words};
    const ROWS: usize = 150;
    let null_words = payload_null_words(ncolumns);
    let keys_values: Vec<i32> = (0..ROWS as i32).map(|row| row * 7 % 50).collect();
    let keys = [ColumnView::try_new(&keys_values, None)?];
    let hashes: Vec<u32> = keys_values.iter().map(|&value| hash_i32(value)).collect();
    let values: Vec<Vec<u64>> = (0..ncolumns)
        .map(|column| {
            (0..ROWS as u64)
                .map(|row| row * 1000 + column as u64)
                .collect()
        })
        .collect();
    let nulls: Vec<Vec<bool>> = (0..ncolumns)
        .map(|column| (0..ROWS).map(|row| row % (3 + column) == 0).collect())
        .collect();
    // The same payload row by row, as a caller would lay it out.
    let width = null_words + ncolumns;
    let mut payload = vec![0u64; ROWS * width];
    for row in 0..ROWS {
        for column in 0..ncolumns {
            if nulls[column][row] {
                payload[row * width + column / 64] |= 1 << (column % 64);
            } else {
                payload[row * width + null_words + column] = values[column][row];
            }
        }
    }
    let payload_bytes: Vec<u8> = payload.iter().flat_map(|word| word.to_ne_bytes()).collect();
    let config = TableConfig {
        keys: &[KeyKind::Int32],
        payload_size: 8 * width,
    };
    // Every third row left out of the mask.
    let mut selected = all_rows(ROWS);
    for row in (0..ROWS).step_by(3) {
        selected[row / 64] &= !(1 << (row % 64));
    }
    let value_slices: Vec<&[u64]> = values.iter().map(Vec::as_slice).collect();
    let null_slices: Vec<&[bool]> = nulls.iter().map(Vec::as_slice).collect();
    let columns = PayloadColumns::new(&value_slices, &null_slices, ROWS)?;
    let mut tables = Vec::new();
    for from_columns in [false, true] {
        let mut table = LocalTable::new(&config, 0, CHUNK)?;
        let mut pending_words = selected.clone();
        let mut offsets = vec![0u32; ROWS];
        loop {
            let chunk = match table.chunks() {
                0 => table.add_chunk()?,
                n => n - 1,
            };
            let shared = table.table()?;
            let mut pending = RowMask::try_new(ROWS, &mut pending_words)?;
            if from_columns {
                shared.append_columns(
                    chunk,
                    &hashes,
                    &keys[..],
                    &columns,
                    &mut pending,
                    &mut offsets,
                )?;
            } else {
                shared.append(
                    chunk,
                    &hashes,
                    &keys[..],
                    Some(&payload_bytes),
                    &mut pending,
                    &mut offsets,
                )?;
            }
            if pending_words.iter().all(|&word| word == 0) {
                break;
            }
            table.add_chunk()?;
        }
        tables.push((table, offsets));
    }
    let (by_rows, offsets) = &tables[0];
    let (by_columns, column_offsets) = &tables[1];
    assert_eq!(
        offsets, column_offsets,
        "the records lie in the same places"
    );
    let (rows_table, columns_table) = (by_rows.table()?, by_columns.table()?);
    for row in (0..ROWS).filter(|row| row % 3 != 0) {
        let expected = rows_table.record(offsets[row])?;
        let got = columns_table.record(column_offsets[row])?;
        assert_eq!(got.payload, expected.payload, "row {row}");
        assert_eq!(got.keys, expected.keys);
        assert_eq!(got.hash, expected.hash);
    }
    // Refusals: a column shorter than the batch, a payload of another size.
    let short: Vec<&[u64]> = vec![&values[0][..ROWS - 1]; ncolumns];
    assert!(PayloadColumns::new(&short, &null_slices, ROWS).is_err());
    let two = PayloadColumns::new(&value_slices[..2], &null_slices[..2], ROWS)?;
    let mut table = LocalTable::new(&config, 0, CHUNK)?;
    let chunk = table.add_chunk()?;
    let mut pending_words = selected.clone();
    let mut pending = RowMask::try_new(ROWS, &mut pending_words)?;
    let mut offsets = vec![0u32; ROWS];
    assert!(
        table
            .table()?
            .append_columns(chunk, &hashes, &keys[..], &two, &mut pending, &mut offsets)
            .is_err()
    );
    Ok(())
}

/// A scattered gather reads what a gather reads, records in any order and
/// a mask with holes, and leaves the rows outside the mask alone.
#[test]
fn a_scattered_gather_reads_what_a_gather_reads() -> Result<()> {
    const ROWS: usize = 200;
    let values: Vec<i32> = (0..ROWS as i32).collect();
    let keys = [ColumnView::try_new(&values, None)?];
    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
    let payload: Vec<u8> = (0..ROWS as u64)
        .flat_map(|row| [row * 11, row << 20].into_iter().flat_map(u64::to_ne_bytes))
        .collect();
    let config = TableConfig {
        keys: &[KeyKind::Int32],
        payload_size: 16,
    };
    let mut table = LocalTable::new(&config, 0, CHUNK)?;
    let mut pending_words = all_rows(ROWS);
    let mut offsets = vec![0u32; ROWS];
    loop {
        let chunk = match table.chunks() {
            0 => table.add_chunk()?,
            n => n - 1,
        };
        let mut pending = RowMask::try_new(ROWS, &mut pending_words)?;
        table.table()?.append(
            chunk,
            &hashes,
            &keys[..],
            Some(&payload),
            &mut pending,
            &mut offsets,
        )?;
        if pending_words.iter().all(|&word| word == 0) {
            break;
        }
        table.add_chunk()?;
    }
    assert!(table.chunks() > 1, "the records span several chunks");
    // The records in a shuffled order, every fifth row left out.
    let shuffled: Vec<u32> = (0..ROWS).map(|row| offsets[row * 7 % ROWS]).collect();
    let mut selected = all_rows(ROWS);
    for row in (0..ROWS).step_by(5) {
        selected[row / 64] &= !(1 << (row % 64));
    }
    let rows = RowMaskView::try_new(ROWS, &selected)?;
    let shared = table.table()?;
    for at in [0, 8] {
        let mut plain = vec![u64::MAX; ROWS];
        let mut scattered = vec![u64::MAX; ROWS];
        shared.gather(&shuffled, &rows, at, &mut plain)?;
        shared.gather_scattered(&shuffled, &rows, at, &mut scattered)?;
        assert_eq!(plain, scattered, "word at {at}");
        for (row, &got) in scattered.iter().enumerate() {
            let expected = if row % 5 == 0 {
                u64::MAX
            } else if at == 0 {
                (row * 7 % ROWS) as u64 * 11
            } else {
                ((row * 7 % ROWS) as u64) << 20
            };
            assert_eq!(got, expected, "row {row}, word at {at}");
        }
    }
    let mut out = vec![0u64; ROWS];
    assert!(
        shared
            .gather_scattered(&shuffled, &rows, 16, &mut out)
            .is_err()
    );
    Ok(())
}
