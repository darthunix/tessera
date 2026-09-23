#![forbid(unsafe_code)]

use anyhow::Result;
use tessera_kernels::table::{
    FORMAT_VERSION, KeyKind, MAX_KEYS, TableConfig, TableMut, region_size,
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
