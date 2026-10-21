//! The format of the index and its records: their sizes, the checks of
//! a configuration, and a header damaged field by field.

use anyhow::Result;

use super::*;
use crate::table::testing::{Column, Keys};

/// A named way to damage an index.
type Corruption<'a> = (&'a str, Rule, &'a dyn Fn(&mut [u64]));

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
    assert_eq!(
        refusal(record_bytes(&TableConfig {
            nkeys: 0,
            payload_size: 8
        })),
        Some((Kind::WrongCall, Rule::KeyCount))
    );
    assert_eq!(
        refusal(record_bytes(&TableConfig {
            nkeys: MAX_KEYS + 1,
            payload_size: 8
        })),
        Some((Kind::WrongCall, Rule::KeyCount))
    );
    assert_eq!(
        refusal(record_bytes(&TableConfig {
            nkeys: 1,
            payload_size: usize::MAX
        })),
        Some((Kind::WrongCall, Rule::RecordFit))
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
    assert_eq!(
        refusal(index_size(&no_keys, 1)),
        Some((Kind::WrongCall, Rule::KeyCount))
    );
    assert_eq!(
        refusal(local(&no_keys, 1)),
        Some((Kind::WrongCall, Rule::KeyCount))
    );
    let too_many = TableConfig {
        nkeys: MAX_KEYS + 1,
        payload_size: 0,
    };
    assert_eq!(
        refusal(index_size(&too_many, 1)),
        Some((Kind::WrongCall, Rule::KeyCount))
    );
    let most = TableConfig {
        nkeys: MAX_KEYS,
        payload_size: 0,
    };
    assert!(local(&most, 1).is_ok());
    let huge_payload = TableConfig {
        nkeys: 1,
        payload_size: usize::MAX - 100,
    };
    assert_eq!(
        refusal(index_size(&huge_payload, 1)),
        Some((Kind::WrongCall, Rule::RecordFit))
    );
    let wide_payload = TableConfig {
        nkeys: 1,
        payload_size: 1 << 33,
    };
    assert_eq!(
        refusal(index_size(&wide_payload, 1)),
        Some((Kind::WrongCall, Rule::RecordFit))
    );
    for bad in [0, 4, 12, MAX_CHUNK_LEN + 8] {
        assert_eq!(
            refusal(OwnedTable::new(&ONE_INT4, 1, bad)),
            Some((Kind::WrongCall, Rule::Block)),
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
        ("magic", Rule::Magic, &|w| {
            set_u64(w, MAGIC, 0x5445_5353_5f54_4142)
        }),
        ("version", Rule::Version, &|w| {
            set_u32(w, VERSION, FORMAT_VERSION + 1)
        }),
        ("header size", Rule::HeaderSize, &|w| {
            set_u32(w, HEADER_SIZE, 88)
        }),
        ("index longer than given", Rule::IndexLength, &|w| {
            set_u64(w, REGION_LEN, len + 8)
        }),
        ("index shorter than buckets", Rule::Buckets, &|w| {
            set_u64(w, REGION_LEN, len - 8)
        }),
        ("index not a multiple of 8", Rule::IndexShape, &|w| {
            set_u64(w, REGION_LEN, len - 4)
        }),
        ("buckets moved", Rule::Buckets, &|w| {
            set_u64(w, BUCKETS_OFFSET, HEADER as u64 + 8)
        }),
        ("buckets misaligned", Rule::Buckets, &|w| {
            set_u64(w, BUCKETS_OFFSET, HEADER as u64 - 4)
        }),
        ("reserved word", Rule::Reserved, &|w| {
            set_u64(w, RESERVED_USED, 8)
        }),
        ("first of the last reserved words", Rule::Reserved, &|w| {
            set_u64(w, RESERVED, 1)
        }),
        ("a byte of the last reserved words", Rule::Reserved, &|w| {
            set_bytes(w, RESERVED + 9, &[3])
        }),
        ("last reserved word", Rule::Reserved, &|w| {
            set_u64(w, RESERVED + 16, 1)
        }),
        ("buckets not a power of two", Rule::BucketCount, &|w| {
            set_u32(w, NBUCKETS, 1000)
        }),
        ("too few buckets", Rule::BucketCount, &|w| {
            set_u32(w, NBUCKETS, 512)
        }),
        ("bucket shift", Rule::BucketShift, &|w| {
            set_u32(w, BUCKET_SHIFT, 21)
        }),
        ("record size", Rule::RecordSize, &|w| {
            set_u32(w, RECORD_SIZE, 48)
        }),
        ("no keys", Rule::KeyCount, &|w| set_u32(w, NKEYS, 0)),
        ("too many keys", Rule::KeyCount, &|w| {
            set_u32(w, NKEYS, MAX_KEYS as u32 + 1)
        }),
        ("flags", Rule::Flags, &|w| set_u32(w, FLAGS, 1)),
    ];
    for &(what, rule, corrupt) in corruptions {
        let mut table = local(&config, 100)?;
        corrupt(table.index_words());
        assert_eq!(
            refusal(table.table()),
            Some((Kind::Damaged, rule)),
            "{what}"
        );
    }
    let mut table = local(&config, 100)?;
    set_u64(table.index_words(), NRECORDS, 12);
    assert!(
        table.table().is_ok(),
        "the record count is the links', not checked against chunks"
    );
    Ok(())
}

/// The keys 0 to `count - 1` as one column, and their hashes.
fn int_keys(count: usize) -> (Vec<i32>, Vec<u32>) {
    let values: Vec<i32> = (0..count as i32).collect();
    let hashes = values.iter().map(|&value| hash_i32(value)).collect();
    (values, hashes)
}

/// A chunk of the longest length takes records up to its last place, and
/// the reference of the last one names its byte.
#[test]
#[cfg_attr(miri, ignore = "inserts 43691 rows")]
fn the_longest_chunk_fills_to_its_last_record() -> Result<()> {
    // Records of 24 bytes: 43690 fill the chunk after its used mark.
    let per_chunk = (MAX_CHUNK_LEN - CHUNK_HEADER) / 24;
    let (values, hashes) = int_keys(per_chunk + 1);
    let keys = [Column::new(&values, None)];
    let mut table = OwnedTable::new(&KEY_ONLY, values.len() as u64, MAX_CHUNK_LEN)?;
    let refs = insert_all(&mut table, &hashes, &Keys(&keys), None)?;
    assert_eq!(table.chunks(), 2);
    let last = CHUNK_HEADER + (per_chunk - 1) * 24;
    assert_eq!(placement(refs[per_chunk - 1]), (0, last));
    assert_eq!(placement(refs[per_chunk]), (1, CHUNK_HEADER));
    let shared = table.table()?;
    assert_eq!(
        shared.record(refs[per_chunk - 1])?.keys,
        [per_chunk as i64 - 1]
    );
    let (found, matches) = probe_all(&shared, &hashes, &Keys(&keys))?;
    assert_eq!((found.len(), matches), (values.len(), refs));
    Ok(())
}

/// A table takes records in its last chunk, number 32767, whose
/// references fill the top bits of a reference, and no chunk past it.
#[test]
#[cfg_attr(miri, ignore = "adds 32768 chunks")]
fn the_last_chunk_holds_records_found_by_their_references() -> Result<()> {
    let (values, hashes) = int_keys(MAX_CHUNKS);
    let keys = [Column::new(&values, None)];
    // Chunks of one record each.
    let mut table = OwnedTable::new(&KEY_ONLY, MAX_CHUNKS as u64, CHUNK_HEADER + 24)?;
    let refs = insert_all(&mut table, &hashes, &Keys(&keys), None)?;
    assert_eq!(table.chunks(), MAX_CHUNKS);
    assert_eq!(
        placement(refs[MAX_CHUNKS - 1]),
        (MAX_CHUNKS - 1, CHUNK_HEADER)
    );
    let shared = table.table()?;
    let (found, matches) = probe_all(&shared, &hashes, &Keys(&keys))?;
    assert_eq!((found.len(), matches), (MAX_CHUNKS, refs));
    assert_eq!(
        refusal(table.add_chunk()),
        Some((Kind::WrongCall, Rule::ChunkLimit))
    );
    Ok(())
}

/// The largest record, one key and a payload that fills a chunk of the
/// longest length, goes in and comes back whole.
#[test]
#[cfg_attr(miri, ignore = "copies records of a mebibyte")]
fn the_largest_record_goes_in_and_comes_back_whole() -> Result<()> {
    let payload_size = MAX_CHUNK_LEN - CHUNK_HEADER - 16 - 8;
    let config = TableConfig {
        nkeys: 1,
        payload_size,
    };
    let (values, hashes) = int_keys(2);
    let keys = [Column::new(&values, None)];
    let payload: Vec<u8> = (0..2 * payload_size)
        .map(|byte| (byte % 251) as u8)
        .collect();
    let mut table = OwnedTable::new(&config, 2, MAX_CHUNK_LEN)?;
    let refs = insert_all(&mut table, &hashes, &Keys(&keys), Some(&payload))?;
    assert_eq!(table.chunks(), 2, "a record a chunk");
    let shared = table.table()?;
    for (row, &reference) in refs.iter().enumerate() {
        let record = shared.record(reference)?;
        assert_eq!(record.keys, [row as i64]);
        assert!(record.payload == &payload[row * payload_size..(row + 1) * payload_size]);
    }
    let (found, matches) = probe_all(&shared, &hashes, &Keys(&keys))?;
    assert_eq!((found, matches), (vec![0, 1], refs));
    Ok(())
}
