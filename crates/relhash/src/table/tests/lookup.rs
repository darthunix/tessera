//! Rows into the table and back: appends and links, probes, steps to
//! the next match, words probed at once, gathers, and what each call
//! refuses.

use anyhow::Result;
use proptest::prelude::*;
use proptest::sample::select;

use super::*;
use crate::table::testing::{Column, Keys};

#[test]
fn inserted_rows_are_found_and_absent_keys_are_not() -> Result<()> {
    let values: Vec<i32> = (0..200).collect();
    let keys = [Column::new(&values, None)];
    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
    let payload = payload_for(200);
    let mut table = local(&ONE_INT4, 200)?;
    let refs = insert_all(&mut table, &hashes, &Keys(&keys), Some(&payload))?;
    assert_eq!(
        table.chunks(),
        2,
        "a record of one key and eight payload bytes takes 32 bytes: 127 per chunk"
    );
    let table = table.table()?;
    assert_eq!(table.stats().records, 200);
    assert_eq!(table.stats().bytes_used, HEADER as u64 + 1024 * 4);
    let mut seen = refs.clone();
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(seen.len(), 200, "every record has its own reference");
    assert_eq!(placement(refs[126]), (0, CHUNK_HEADER + 126 * 32));
    assert_eq!(
        placement(refs[127]),
        (1, CHUNK_HEADER),
        "the next chunk takes the rest"
    );
    for (row, &offset) in refs.iter().enumerate() {
        let record = table.record(offset)?;
        assert_eq!(record.hash, hashes[row]);
        assert_eq!(record.null_bits, 0);
        assert_eq!(record.keys, &[row as i64]);
        assert_eq!(record.payload, (row as u64 * 10).to_ne_bytes());
    }
    let (found, matches) = probe_all(&table, &hashes, &Keys(&keys))?;
    assert_eq!(found, (0..200).collect::<Vec<_>>());
    assert_eq!(matches, refs);

    let absent: Vec<i32> = (1000..1200).collect();
    let absent_keys = [Column::new(&absent, None)];
    let absent_hashes: Vec<u32> = absent.iter().map(|&value| hash_i32(value)).collect();
    let (found, _) = probe_all(&table, &absent_hashes, &Keys(&absent_keys))?;
    assert!(found.is_empty());

    let every_third: Vec<u64> = all_rows(200)
        .iter()
        .map(|word| word & 0x9249_2492_4924_9249)
        .collect();
    let looked_up = rows_of(&RowMaskView::try_new(200, &every_third)?);
    let mut rows = every_third.clone();
    let mut matched = vec![0; 200];
    let mut matches = Matches::new(&mut rows, &mut matched)?;
    assert_eq!(
        table.probe(&hashes, &Keys(&keys), &mut matches)?,
        looked_up.len()
    );
    assert_eq!(rows, every_third, "the rows not looked up stay out");
    for row in looked_up {
        assert_eq!(matched[row], refs[row]);
    }
    Ok(())
}

#[test]
fn equal_keys_chain_through_next_match() -> Result<()> {
    let values: Vec<i32> = (0..100).map(|row| row % 10).collect();
    let keys = [Column::new(&values, None)];
    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
    let payload = payload_for(100);
    let mut table = local(&ONE_INT4, 100)?;
    insert_all(&mut table, &hashes, &Keys(&keys), Some(&payload))?;
    let table = table.table()?;

    let (found, matches) = probe_all(&table, &hashes, &Keys(&keys))?;
    assert_eq!(found.len(), 100);
    let mut chains: Vec<Vec<u32>> = (0..10).map(|key| vec![matches[key]]).collect();
    let mut current = matches[..10].to_vec();
    let mut rows = all_rows(10);
    let mut more = Matches::new(&mut rows, &mut current)?;
    while table.next_match(&mut more)? > 0 {
        for key in more.rows().selected_rows() {
            chains[key].push(more.refs()[key]);
        }
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

/// A NULL key and the value it hashes like, as a grouping hashes a NULL,
/// find records of their own; a NULL the caller leaves out of the batch,
/// as a join does, gets none.
#[test]
fn a_null_key_groups_apart_from_the_value_it_hashes_like() -> Result<()> {
    let values = [GROUP_KEY, 0];
    let non_null_words = [0b01];
    let keys = [Column::new(&values, Some(&non_null_words))];
    let hashes = [hash_i32(GROUP_KEY); 2];
    let valid = [0b11];

    let mut table = local(&ONE_INT4, 2)?;
    let mut refs = [0; 2];
    table.insert(
        &hashes,
        &Keys(&keys),
        RowMaskView::try_new(2, &valid)?,
        None,
        &mut refs,
    )?;
    let table = table.table()?;
    assert_eq!(table.stats().records, 2);
    let (found, matches) = probe_all(&table, &hashes, &Keys(&keys))?;
    assert_eq!(found, [0, 1]);
    assert_ne!(matches[0], matches[1], "each row finds its own record");
    let value = table.record(matches[0])?;
    assert_eq!(
        (value.null_bits, value.keys),
        (0, &[i64::from(GROUP_KEY)][..])
    );
    let null = table.record(matches[1])?;
    assert_eq!((null.null_bits, null.keys), (1, &[0][..]));
    let mut more_words = [0b11];
    let mut next = matches.clone();
    let mut more = Matches::new(&mut more_words, &mut next)?;
    assert_eq!(
        table.next_match(&mut more)?,
        0,
        "neither has a second record"
    );
    assert_eq!(more_words, [0]);
    assert_eq!(next, matches, "references without a next record stay");

    let rejected = [0b01];
    let mut table = local(&ONE_INT4, 2)?;
    table.insert(
        &hashes,
        &Keys(&keys),
        RowMaskView::try_new(2, &rejected)?,
        None,
        &mut refs,
    )?;
    let table = table.table()?;
    assert_eq!(
        table.stats().records,
        1,
        "the rejected NULL row is not inserted"
    );
    let (found, _) = probe_all(&table, &hashes, &Keys(&keys))?;
    assert_eq!(found, [0]);
    Ok(())
}

/// A key cleared in every record holds 0, NULL or not, its NULL bit
/// kept, and the other key and the payload are untouched; the records of
/// several chunks are all visited, and a key past the table's is refused.
#[test]
fn a_cleared_key_is_zero_in_every_record() -> Result<()> {
    let config = TableConfig {
        nkeys: 2,
        payload_size: 8,
    };
    let nrows: usize = 300;
    let first: Vec<i32> = (0..nrows).map(|row| row as i32 % 7 - 3).collect();
    let second: Vec<i64> = (0..nrows)
        .map(|row| row as i64 * 1_000_000_007 - 5)
        .collect();
    let non_null = words(&(0..nrows).map(|row| row % 3 != 1).collect::<Vec<_>>());
    let non_nulls = RowMaskView::try_new(nrows, &non_null)?;
    let hashes: Vec<u32> = (0..nrows as u32).collect();
    let keys = [
        Column::new(&first, None),
        Column::new(&second, Some(&non_null)),
    ];
    let payload = payload_for(nrows);
    let mut table = OwnedTable::new(&config, nrows as u64, CHUNK_HEADER + 64 * 48)?;
    let refs = insert_all(&mut table, &hashes, &Keys(&keys), Some(&payload))?;
    assert!(table.chunks() > 1, "the records span chunks");
    assert_eq!(table.table_mut()?.clear_key(1)?, nrows as u64);
    assert!(table.table_mut()?.clear_key(2).is_err());
    let table = table.table()?;
    for (row, &offset) in refs.iter().enumerate() {
        let record = table.record(offset)?;
        let null = !non_nulls.contains(row)?;
        assert_eq!(record.keys, &[i64::from(first[row]), 0], "row {row}");
        assert_eq!(record.null_bits, u32::from(null) << 1, "row {row}");
        assert_eq!(record.payload, &payload[row * 8..row * 8 + 8], "row {row}");
    }
    Ok(())
}

#[test]
fn two_keys_of_different_kinds_compare_whole() -> Result<()> {
    let config = TableConfig {
        nkeys: 2,
        payload_size: 0,
    };
    let first: Vec<i32> = (0..100).map(|row| row % 7 - 3).collect();
    let second: Vec<i64> = (0..100).map(|row| row * 1_000_000_007 - 5).collect();
    let hashes: Vec<u32> = first
        .iter()
        .zip(&second)
        .map(|(&a, &b)| hash_combine(hash_i32(a), hash_i64(b)))
        .collect();
    let keys = [Column::new(&first, None), Column::new(&second, None)];
    let mut table = local(&config, 100)?;
    let refs = insert_all(&mut table, &hashes, &Keys(&keys), None)?;
    let table = table.table()?;
    let record = table.record(refs[5])?;
    assert_eq!(record.keys, &[i64::from(first[5]), second[5]]);
    assert!(record.payload.is_empty());
    let (found, matches) = probe_all(&table, &hashes, &Keys(&keys))?;
    assert_eq!(found.len(), 100);
    assert_eq!(matches, refs);

    let other_second: Vec<i64> = second.iter().map(|value| value + 1).collect();
    let keys = [Column::new(&first, None), Column::new(&other_second, None)];
    let (found, _) = probe_all(&table, &hashes, &Keys(&keys))?;
    assert!(
        found.is_empty(),
        "the same hash and first key are not enough"
    );
    let other_first: Vec<i32> = first.iter().map(|value| value + 1).collect();
    let keys = [Column::new(&other_first, None), Column::new(&second, None)];
    let (found, _) = probe_all(&table, &hashes, &Keys(&keys))?;
    assert!(found.is_empty());
    Ok(())
}

#[test]
fn a_full_chunk_leaves_the_rest_pending_until_linked_elsewhere() -> Result<()> {
    let values: Vec<i32> = (0..100).collect();
    let keys = [Column::new(&values, None)];
    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
    // A chunk of exactly sixteen records of 32 bytes after its used mark.
    let mut table = OwnedTable::new(&ONE_INT4, 100, CHUNK_HEADER + 16 * 32)?;
    let chunk = table.add_chunk()?;
    let mut pending_words = all_rows(100);
    let mut refs = vec![0; 100];
    {
        let shared = table.table()?;
        let source = Keys(&keys);
        let mut batch = Batch::new(&hashes, &source, &mut pending_words, &mut refs)?;
        let appended = shared.append(chunk, None, &mut batch)?;
        assert_eq!(appended, 16, "a chunk of sixteen records holds sixteen");
        assert_eq!(rows_of(&batch.pending()), (16..100).collect::<Vec<_>>());
        assert_eq!(shared.append(chunk, None, &mut batch)?, 0);
        assert_eq!(
            shared.stats().records,
            0,
            "appended records are not linked yet"
        );
        let (found, _) = probe_all(&shared, &hashes, &Keys(&keys))?;
        assert!(found.is_empty(), "nor found");
        let mut from = CHUNK_HEADER;
        assert_eq!(shared.link(chunk, &mut from)?, 16);
        assert_eq!(from, CHUNK_HEADER + 16 * 32);
        assert_eq!(shared.link(chunk, &mut from)?, 0, "nothing is linked twice");
        assert_eq!(shared.stats().records, 16);
        let (found, matches) = probe_all(&shared, &hashes, &Keys(&keys))?;
        assert_eq!(found, (0..16).collect::<Vec<_>>());
        assert_eq!(matches[..16], refs[..16]);
    }
    assert_eq!(table.chunk_words(chunk)[0], (CHUNK_HEADER + 16 * 32) as u64);
    // The rest go into a second chunk and are found alongside.
    let second = table.add_chunk()?;
    let shared = table.table()?;
    shared.append(
        second,
        None,
        &mut Batch::new(&hashes, &Keys(&keys), &mut pending_words, &mut refs)?,
    )?;
    let mut from = CHUNK_HEADER;
    shared.link(second, &mut from)?;
    assert_eq!(placement(refs[16]), (second, CHUNK_HEADER));
    let (found, _) = probe_all(&shared, &hashes, &Keys(&keys))?;
    assert_eq!(found, (0..32).collect::<Vec<_>>());
    // Appending to or linking a chunk that does not exist, or linking from
    // inside a record or past the used mark, fails.
    assert!(
        shared
            .append(
                9,
                None,
                &mut Batch::new(&hashes, &Keys(&keys), &mut pending_words, &mut refs).unwrap()
            )
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
    let mut table = OwnedTable::new(&ONE_INT4, 100, 4096)?;
    let chunks = [table.add_chunk()?, table.add_chunk()?];
    let shared = table.table()?;
    for (chunk, values) in chunks.iter().zip([&first, &second]) {
        let keys = [Column::new(values, None)];
        let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
        let mut pending_words = all_rows(values.len());
        let mut refs = vec![0; values.len()];
        shared.append(
            *chunk,
            None,
            &mut Batch::new(&hashes, &Keys(&keys), &mut pending_words, &mut refs)?,
        )?;
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
        nkeys: 1,
        payload_size: 64,
    };
    let values = [1];
    let keys = [Column::new(&values, None)];
    let mut table = OwnedTable::new(&config, 1, CHUNK_HEADER + 64)?;
    let result = table.insert(
        &[hash_i32(1)],
        &Keys(&keys),
        RowMaskView::try_new(1, &[1])?,
        None,
        &mut [0],
    );
    assert_eq!(refusal(result), Some((Kind::WrongCall, Rule::RecordRoom)));

    // A table is not even sized for a record that no chunk can hold: one
    // key and the largest payload fill a chunk of 1 MiB after its used
    // mark, eight bytes more do not.
    let largest = MAX_CHUNK_LEN - CHUNK_HEADER - 16 - 8;
    assert_eq!(
        record_bytes(&TableConfig {
            nkeys: 1,
            payload_size: largest
        })?,
        MAX_CHUNK_LEN - CHUNK_HEADER
    );
    for payload_size in [largest + 1, MAX_CHUNK_LEN] {
        let result = record_bytes(&TableConfig {
            nkeys: 1,
            payload_size,
        });
        assert_eq!(refusal(result), Some((Kind::WrongCall, Rule::RecordFit)));
        let config = TableConfig {
            nkeys: 1,
            payload_size,
        };
        assert!(index_size(&config, 1).is_err());
    }
    Ok(())
}

#[test]
fn batches_of_every_shape_round_trip() -> Result<()> {
    for nrows in [0, 1, 63, 64, 65, 200, 1000] {
        let values: Vec<i32> = (0..nrows as i32).map(|row| row * 3 - 1000).collect();
        let keys = [Column::new(&values, None)];
        let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
        let payload = payload_for(nrows);
        let mut table = local(&ONE_INT4, nrows as u64)?;
        let refs = insert_all(&mut table, &hashes, &Keys(&keys), Some(&payload))?;
        let table = table.table()?;
        assert_eq!(table.stats().records, nrows as u64, "{nrows} rows");
        let (found, matches) = probe_all(&table, &hashes, &Keys(&keys))?;
        assert_eq!(found, (0..nrows).collect::<Vec<_>>(), "{nrows} rows");
        assert_eq!(matches, refs, "{nrows} rows");
        if nrows > 0 {
            let last = table.record(refs[nrows - 1])?;
            assert_eq!(last.payload, ((nrows as u64 - 1) * 10).to_ne_bytes());
        }
    }
    Ok(())
}

#[test]
fn a_missing_payload_is_zeros() -> Result<()> {
    let values = [42];
    let keys = [Column::new(&values, None)];
    let hashes = [hash_i32(42)];
    let config = TableConfig {
        nkeys: 1,
        payload_size: 13,
    };
    let mut table = local(&config, 1)?;
    let refs = insert_all(&mut table, &hashes, &Keys(&keys), None)?;
    assert_eq!(table.table()?.record(refs[0])?.payload, [0; 13]);
    Ok(())
}

#[test]
fn dimension_errors_come_before_any_change() -> Result<()> {
    let config = TableConfig {
        nkeys: 2,
        payload_size: 8,
    };
    let values: Vec<i32> = (0..70).collect();
    let one_key = [Column::new(&values, None)];
    let two_keys = [Column::new(&values, None), Column::new(&values, None)];
    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
    let payload = payload_for(70);
    let mut table = local(&config, 70)?;
    let chunk = table.add_chunk()?;
    let mut pending_words = all_rows(70);
    let mut refs = vec![0; 70];
    let rows_words = all_rows(70);
    let rows = RowMaskView::try_new(70, &rows_words)?;
    {
        let shared = table.table()?;
        assert!(
            shared
                .append(
                    chunk,
                    None,
                    &mut Batch::new(&hashes, &Keys(&one_key), &mut pending_words, &mut refs)
                        .unwrap()
                )
                .is_err()
        );
        // Row counts that differ are refused as the batch is made.
        let two = Keys(&two_keys);
        assert!(Batch::new(&hashes[..69], &two, &mut pending_words, &mut refs).is_err());
        assert!(Batch::new(&hashes, &two, &mut pending_words, &mut refs[..69]).is_err());
        let mask = RowMask::try_new(70, &mut pending_words)?;
        assert!(Batch::from_mask(&hashes, &two, mask, &mut refs[..69]).is_err());
        assert!(
            shared
                .append(
                    chunk,
                    Some(&payload[..8]),
                    &mut Batch::new(&hashes, &two, &mut pending_words, &mut refs).unwrap()
                )
                .is_err()
        );
        assert_eq!(pending_words, all_rows(70), "no row left the batch");
        assert_eq!(shared.stats().records, 0);
        let mut found_words = rows_words.clone();
        let mut matches = vec![0; 70];
        let mut found = Matches::new(&mut found_words, &mut matches)?;
        assert!(shared.probe(&hashes, &Keys(&one_key), &mut found).is_err());
        assert!(
            shared
                .probe(&hashes[..69], &Keys(&two_keys), &mut found)
                .is_err()
        );
        // A mask and references of other lengths are refused as the
        // matches are made.
        assert!(Matches::new(&mut found_words, &mut refs[..69]).is_err());
        assert!(MatchesView::new(rows, &refs[..69]).is_err());
    }
    assert_eq!(
        table.chunk_words(chunk)[0],
        CHUNK_HEADER as u64,
        "the chunk took nothing"
    );
    assert_eq!(
        table.insert(
            &hashes,
            &Keys(&two_keys),
            RowMaskView::try_new(70, &pending_words)?,
            Some(&payload),
            &mut refs,
        )?,
        70
    );
    Ok(())
}

/// Probe the first row of a batch through a fresh attachment.
fn table_probe_one(
    table: &OwnedTable,
    hashes: &[u32],
    keys: &[Column],
) -> crate::error::Result<bool> {
    let table = table.table()?;
    let mut rows = [1];
    let mut refs = vec![0; hashes.len()];
    let mut matches = Matches::new(&mut rows, &mut refs)?;
    Ok(table.probe(hashes, &Keys(keys), &mut matches)? == 1)
}

#[test]
fn a_corrupt_reference_chain_or_used_mark_is_an_error() -> Result<()> {
    let values: Vec<i32> = (0..5).collect();
    let keys = [Column::new(&values, None)];
    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
    let mut table = local(&ONE_INT4, 5)?;
    let refs = insert_all(&mut table, &hashes, &Keys(&keys), None)?;
    assert!(table_probe_one(&table, &hashes, &keys)?);
    let bucket = HEADER + (hashes[0] >> 22) as usize * 4;
    let head = table.index_words()[bucket / 8];
    for (what, damaged) in [
        ("a chunk that does not exist", reference(3, CHUNK_HEADER)),
        ("a chunk's used mark", reference(0, 0) | 1 << UNIT_BITS),
        ("past the chunk", reference(0, CHUNK - 8)),
        ("inside a record", refs[0] + 1),
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
        table.table()?.record(refs[0] + 1).is_err(),
        "a reference inside a record"
    );

    // The record of key 0 chains to itself: a probe with its hash and an
    // absent key walks the cycle and gives up.
    let (chunk, byte) = placement(refs[0]);
    set_u32(table.chunk_words(chunk), byte + 4, refs[0]);
    let absent = [999; 5];
    let absent_keys = [Column::new(&absent, None)];
    let result = table_probe_one(&table, &hashes, &absent_keys);
    assert_eq!(refusal(result), Some((Kind::Damaged, Rule::Chain)));
    // A count of records far past what the chunks hold does not keep the
    // walk going around the loop: the places of the chunks bound it too.
    // Miri would take minutes over the 2^17 steps of the walk, the steps
    // above again; the bound has a test of its own.
    if !cfg!(miri) {
        set_u64(table.index_words(), NRECORDS, u64::MAX);
        let result = table_probe_one(&table, &hashes, &absent_keys);
        assert_eq!(refusal(result), Some((Kind::Damaged, Rule::Chain)));
    }

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

#[test]
fn reference_zero_names_no_record() -> Result<()> {
    let values = [7];
    let keys = [Column::new(&values, None)];
    let hashes = [hash_i32(7)];
    let mut table = local(&ONE_INT4, 1)?;
    let refs = insert_all(&mut table, &hashes, &Keys(&keys), None)?;
    assert_ne!(refs[0], 0);
    // The one record ends its chain with 0, and only its bucket is not 0.
    let (chunk, byte) = placement(refs[0]);
    let next = table.chunk_words(chunk)[byte / 8].to_ne_bytes();
    assert_eq!(u32::from_ne_bytes(next[4..].try_into()?), 0);
    let heads: Vec<u32> = table.index_words()[HEADER / 8..]
        .iter()
        .flat_map(|word| {
            let bytes = word.to_ne_bytes();
            [&bytes[..4], &bytes[4..]].map(|half| u32::from_ne_bytes(half.try_into().unwrap()))
        })
        .filter(|&head| head != 0)
        .collect();
    assert_eq!(heads, refs);
    // Reference 0 is chunk 0's used mark: no call takes it for a record.
    let table = table.table()?;
    assert!(table.record(0).is_err());
    let rows = RowMaskView::try_new(1, &[1])?;
    assert!(
        table
            .gather(MatchesView::new(rows, &[0])?, 0, &mut [0])
            .is_err()
    );
    assert!(
        table
            .next_match(&mut Matches::new(&mut [1], &mut [0])?)
            .is_err()
    );
    Ok(())
}

/// Probe `rows` of a batch in one call: the rows found and their matches.
fn probe_rows(
    table: &Table<'_>,
    hashes: &[u32],
    keys: &[Column],
    rows: &[u64],
) -> crate::error::Result<(Vec<u64>, Vec<u32>)> {
    let mut found = rows.to_vec();
    let mut refs = vec![0; hashes.len()];
    table.probe(
        hashes,
        &Keys(keys),
        &mut Matches::new(&mut found, &mut refs)?,
    )?;
    Ok((found, refs))
}

/// Where integer code breaks: both ends of int4, one step inside them,
/// -1, 0 and 1.
const EDGES: [i32; 7] = [i32::MIN, i32::MIN + 1, -1, 0, 1, i32::MAX - 1, i32::MAX];

#[test]
fn a_word_probed_at_once_answers_as_rows_probed_alone() -> Result<()> {
    // Three thousand records over 8192 buckets, and over many chunks, with
    // equal keys and equal hashes of different rows in chains, probed by
    // batches of up to 200 rows; under Miri, which would take minutes, a
    // fifth of the records, over several chunks, and batches of up to 70.
    let (nrecords, distinct, most) = if cfg!(miri) {
        (600, 340, 70)
    } else {
        (3000, 1700, 200)
    };
    let values: Vec<i32> = (0..nrecords).map(|row| row % distinct).collect();
    let keys = [Column::new(&values, None)];
    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
    let mut table = local(&ONE_INT4, nrecords as u64)?;
    insert_all(&mut table, &hashes, &Keys(&keys), None)?;
    let table = table.table()?;
    // Present keys, absent ones past them, and edges.
    let probes = proptest::collection::vec(
        prop_oneof![0..distinct + 300, select(EDGES.to_vec())],
        0..=most,
    );
    property(probes, |probe_values| -> Result<()> {
        let nrows = probe_values.len();
        let probe_keys = [Column::new(&probe_values, None)];
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
                (0..distinct).contains(&probe_values[row]),
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
    let keys = [Column::new(&values, None)];
    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
    let mut table = local(&ONE_INT4, 64)?;
    let refs = insert_all(&mut table, &hashes, &Keys(&keys), None)?;
    // The record of key 0 chains to itself; absent keys with the same
    // hashes walk the cycle in the whole-word path.
    let (chunk, byte) = placement(refs[0]);
    set_u32(table.chunk_words(chunk), byte + 4, refs[0]);
    let absent = [999; 64];
    let absent_keys = [Column::new(&absent, None)];
    let result = probe_rows(&table.table()?, &hashes, &absent_keys, &all_rows(64));
    assert_eq!(refusal(result), Some((Kind::Damaged, Rule::Chain)));
    // As a row alone does, the word stops at the places of the chunks
    // when the count of records is damaged too; not under Miri, for the
    // reason `a_corrupt_reference_chain_or_used_mark_is_an_error` gives.
    if !cfg!(miri) {
        set_u64(table.index_words(), NRECORDS, u64::MAX);
        let result = probe_rows(&table.table()?, &hashes, &absent_keys, &all_rows(64));
        assert_eq!(refusal(result), Some((Kind::Damaged, Rule::Chain)));
    }
    Ok(())
}

#[test]
fn gather_reads_one_payload_word_of_each_selected_match() -> Result<()> {
    let config = TableConfig {
        nkeys: 1,
        payload_size: 16,
    };
    let nrows = 100;
    let values: Vec<i32> = (0..nrows as i32).collect();
    let keys = [Column::new(&values, None)];
    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
    // Payload words: the row times ten, then the row plus a million.
    let payload: Vec<u8> = (0..nrows as u64)
        .flat_map(|row| [row * 10, row + 1_000_000])
        .flat_map(u64::to_ne_bytes)
        .collect();
    let mut table = local(&config, nrows as u64)?;
    insert_all(&mut table, &hashes, &Keys(&keys), Some(&payload))?;
    let table = table.table()?;
    let (_, matches) = probe_all(&table, &hashes, &Keys(&keys))?;

    // Every third row, across both words of the batch.
    let selected: Vec<u64> = (0..2)
        .map(|index| {
            (0..64)
                .filter(|bit| (index * 64 + bit) % 3 == 0 && index * 64 + bit < nrows)
                .fold(0, |word, bit| word | 1 << bit)
        })
        .collect();
    let rows = RowMaskView::try_new(nrows, &selected)?;
    let view = MatchesView::new(rows, &matches)?;
    assert_eq!(
        (view.rows().words().collect::<Vec<_>>(), view.refs()),
        (selected.clone(), &matches[..])
    );
    for (at, expected) in [
        (0, &(|row: u64| row * 10) as &dyn Fn(u64) -> u64),
        (8, &|row: u64| row + 1_000_000),
    ] {
        let mut out = vec![u64::MAX; nrows];
        table.gather(MatchesView::new(rows, &matches)?, at, &mut out)?;
        for (row, &value) in out.iter().enumerate() {
            let wanted = if row % 3 == 0 {
                expected(row as u64)
            } else {
                u64::MAX
            };
            assert_eq!(value, wanted, "word at {at}, row {row}");
        }
    }

    // A word past the payload or not at a multiple of 8, short buffers and
    // a bad reference are errors.
    let mut out = vec![0; nrows];
    assert!(
        table
            .gather(MatchesView::new(rows, &matches)?, 9, &mut out)
            .is_err()
    );
    assert!(
        table
            .gather(MatchesView::new(rows, &matches)?, 4, &mut out)
            .is_err()
    );
    assert!(
        table
            .gather_scattered(MatchesView::new(rows, &matches)?, 4, &mut out)
            .is_err()
    );
    assert!(
        table
            .gather(MatchesView::new(rows, &matches)?, usize::MAX, &mut out)
            .is_err()
    );
    assert!(MatchesView::new(rows, &matches[1..]).is_err());
    assert!(
        table
            .gather(MatchesView::new(rows, &matches)?, 0, &mut out[1..])
            .is_err()
    );
    let mut inside = matches.clone();
    inside[0] += 1;
    assert!(
        table
            .gather(MatchesView::new(rows, &inside)?, 0, &mut out)
            .is_err()
    );
    Ok(())
}

/// A scattered gather, and one of several words, read what a gather reads,
/// records in any order and a mask with holes, and leave the rows outside
/// the mask alone.
#[test]
fn a_scattered_gather_reads_what_a_gather_reads() -> Result<()> {
    const ROWS: usize = 200;
    let values: Vec<i32> = (0..ROWS as i32).collect();
    let keys = [Column::new(&values, None)];
    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
    let payload: Vec<u8> = (0..ROWS as u64)
        .flat_map(|row| [row * 11, row << 20].into_iter().flat_map(u64::to_ne_bytes))
        .collect();
    let config = TableConfig {
        nkeys: 1,
        payload_size: 16,
    };
    let mut table = OwnedTable::new(&config, 0, CHUNK)?;
    let mut pending_words = all_rows(ROWS);
    let mut refs = vec![0u32; ROWS];
    loop {
        let chunk = match table.chunks() {
            0 => table.add_chunk()?,
            n => n - 1,
        };
        table.table()?.append(
            chunk,
            Some(&payload),
            &mut Batch::new(&hashes, &Keys(&keys), &mut pending_words, &mut refs)?,
        )?;
        if pending_words.iter().all(|&word| word == 0) {
            break;
        }
        table.add_chunk()?;
    }
    assert!(table.chunks() > 1, "the records span several chunks");
    // The records in a shuffled order, every fifth row left out.
    let shuffled: Vec<u32> = (0..ROWS).map(|row| refs[row * 7 % ROWS]).collect();
    let mut selected = all_rows(ROWS);
    for row in (0..ROWS).step_by(5) {
        selected[row / 64] &= !(1 << (row % 64));
    }
    let rows = RowMaskView::try_new(ROWS, &selected)?;
    let shared = table.table()?;
    for at in [0, 8] {
        let mut plain = vec![u64::MAX; ROWS];
        let mut scattered = vec![u64::MAX; ROWS];
        shared.gather(MatchesView::new(rows, &shuffled)?, at, &mut plain)?;
        shared.gather_scattered(MatchesView::new(rows, &shuffled)?, at, &mut scattered)?;
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
            .gather_scattered(MatchesView::new(rows, &shuffled)?, 16, &mut out)
            .is_err()
    );
    // Both words in one call read what a word at a time reads.
    let mut first = vec![u64::MAX; ROWS];
    let mut second = vec![u64::MAX; ROWS];
    shared.gather_words(
        MatchesView::new(rows, &shuffled)?,
        0,
        &mut [&mut first, &mut second],
    )?;
    for (word, got) in [first, second].iter().enumerate() {
        let mut expected = vec![u64::MAX; ROWS];
        shared.gather(MatchesView::new(rows, &shuffled)?, 8 * word, &mut expected)?;
        assert_eq!(got, &expected, "word {word}");
    }
    let mut second = vec![u64::MAX; ROWS];
    shared.gather_words(MatchesView::new(rows, &shuffled)?, 1, &mut [&mut second])?;
    let mut expected = vec![u64::MAX; ROWS];
    shared.gather(MatchesView::new(rows, &shuffled)?, 8, &mut expected)?;
    assert_eq!(second, expected);
    // No words, words past the payload, outputs of another row count.
    let (mut a, mut b, mut short) = (vec![0; ROWS], vec![0; ROWS], vec![0; ROWS - 1]);
    assert!(
        shared
            .gather_words(MatchesView::new(rows, &shuffled)?, 0, &mut [])
            .is_err()
    );
    assert!(
        shared
            .gather_words(MatchesView::new(rows, &shuffled)?, 1, &mut [&mut a, &mut b])
            .is_err()
    );
    assert!(
        shared
            .gather_words(
                MatchesView::new(rows, &shuffled)?,
                0,
                &mut [&mut a, &mut short]
            )
            .is_err()
    );
    Ok(())
}
