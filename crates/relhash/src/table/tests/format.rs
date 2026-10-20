//! The format of the index and its records: their sizes, the checks of
//! a configuration, and a header damaged field by field.

use anyhow::Result;

use super::*;

/// A named way to damage an index.
type Corruption<'a> = (&'a str, &'a dyn Fn(&mut [u64]));

#[test]
fn a_created_table_is_empty_and_attaches_again() -> Result<()> {
    let config = TableConfig {
        nkeys: 2,
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
        assert_eq!(table.config(), config);
        assert_eq!(table.payload_size(), 12);
        assert_eq!(table.record_size(), model_record_size(2, 12));
        stats
    };
    let again = table.table()?;
    assert_eq!(again.stats(), stats);
    assert_eq!(again.config(), config);
    assert_eq!(table.chunks(), 0, "no chunk before the first record");
    Ok(())
}

#[test]
fn a_record_takes_its_header_its_key_slots_and_its_payload() -> Result<()> {
    for nkeys in 1..=MAX_KEYS {
        for payload_size in 0..=1024 {
            let bytes = record_bytes(&TableConfig {
                nkeys,
                payload_size,
            })?;
            assert_eq!(bytes, model_record_size(nkeys, payload_size));
        }
    }
    assert!(
        record_bytes(&TableConfig {
            nkeys: 0,
            payload_size: 8
        })
        .is_err()
    );
    assert!(
        record_bytes(&TableConfig {
            nkeys: MAX_KEYS + 1,
            payload_size: 8
        })
        .is_err()
    );
    assert!(
        record_bytes(&TableConfig {
            nkeys: 1,
            payload_size: usize::MAX
        })
        .is_err()
    );
    Ok(())
}

#[test]
fn index_size_covers_header_and_buckets() -> Result<()> {
    for payload_size in [0, 1, 8, 13] {
        let config = TableConfig {
            nkeys: 3,
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
            assert!(OwnedTable::new(&config, capacity, CHUNK).is_ok());
        }
    }
    Ok(())
}

#[test]
fn configurations_are_checked() -> Result<()> {
    let no_keys = TableConfig {
        nkeys: 0,
        payload_size: 0,
    };
    assert!(index_size(&no_keys, 1).is_err());
    assert!(local(&no_keys, 1).is_err());
    let too_many = TableConfig {
        nkeys: MAX_KEYS + 1,
        payload_size: 0,
    };
    assert!(index_size(&too_many, 1).is_err());
    let most = TableConfig {
        nkeys: MAX_KEYS,
        payload_size: 0,
    };
    assert!(local(&most, 1).is_ok());
    let huge_payload = TableConfig {
        nkeys: 1,
        payload_size: usize::MAX - 100,
    };
    assert!(index_size(&huge_payload, 1).is_err());
    let wide_payload = TableConfig {
        nkeys: 1,
        payload_size: 1 << 33,
    };
    assert!(index_size(&wide_payload, 1).is_err());
    for bad in [0, 4, 12, MAX_CHUNK_LEN + 8] {
        assert!(
            OwnedTable::new(&ONE_INT4, 1, bad).is_err(),
            "chunks of {bad} bytes"
        );
    }
    assert!(OwnedTable::new(&ONE_INT4, 1, MAX_CHUNK_LEN).is_ok());
    Ok(())
}

#[test]
fn a_corrupt_header_is_refused() -> Result<()> {
    let config = TableConfig {
        nkeys: 2,
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
        ("first of the last reserved words", &|w| {
            set_u64(w, RESERVED, 1)
        }),
        ("a byte of the last reserved words", &|w| {
            set_bytes(w, RESERVED + 9, &[3])
        }),
        ("last reserved word", &|w| set_u64(w, RESERVED + 16, 1)),
        ("buckets not a power of two", &|w| {
            set_u32(w, NBUCKETS, 1000)
        }),
        ("too few buckets", &|w| set_u32(w, NBUCKETS, 512)),
        ("bucket shift", &|w| set_u32(w, BUCKET_SHIFT, 21)),
        ("record size", &|w| set_u32(w, RECORD_SIZE, 48)),
        ("no keys", &|w| set_u32(w, NKEYS, 0)),
        ("too many keys", &|w| set_u32(w, NKEYS, MAX_KEYS as u32 + 1)),
        ("flags", &|w| set_u32(w, FLAGS, 1)),
    ];
    for (what, corrupt) in corruptions {
        let mut table = local(&config, 100)?;
        corrupt(table.index_words());
        assert!(table.table().is_err(), "{what} was accepted");
    }
    let mut table = local(&config, 100)?;
    set_u64(table.index_words(), NRECORDS, 12);
    assert!(
        table.table().is_ok(),
        "the record count is the links', not checked against chunks"
    );
    Ok(())
}
