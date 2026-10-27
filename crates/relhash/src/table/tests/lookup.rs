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
    assert_eq!(
        refusal(table.table_mut()?.clear_key(2)),
        Some((Kind::WrongCall, Rule::Key))
    );
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
        let mut shared = table.table_mut()?;
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
    let mut shared = table.table_mut()?;
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
    assert_eq!(
        refusal(shared.append(
            9,
            None,
            &mut Batch::new(&hashes, &Keys(&keys), &mut pending_words, &mut refs).unwrap()
        )),
        Some((Kind::WrongCall, Rule::NoChunk))
    );
    let mut from = CHUNK_HEADER;
    assert_eq!(
        refusal(shared.link(9, &mut from)),
        Some((Kind::WrongCall, Rule::NoChunk))
    );
    let mut inside = CHUNK_HEADER + 4;
    assert_eq!(
        refusal(shared.link(chunk, &mut inside)),
        Some((Kind::WrongCall, Rule::Boundary))
    );
    let mut past = CHUNK_HEADER + 17 * 32;
    assert_eq!(
        refusal(shared.link(chunk, &mut past)),
        Some((Kind::WrongCall, Rule::Boundary))
    );
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
    let mut shared = table.table_mut()?;
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
    // The owning table refuses chunks too small for a record as it is
    // made, before a call would add them up to the limit.
    let result = OwnedTable::new(&config, 1, CHUNK_HEADER + 64);
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
        assert_eq!(
            refusal(index_size(&config, 1)),
            Some((Kind::WrongCall, Rule::RecordFit))
        );
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
        let mut shared = table.table_mut()?;
        assert_eq!(
            refusal(shared.append(
                chunk,
                None,
                &mut Batch::new(&hashes, &Keys(&one_key), &mut pending_words, &mut refs).unwrap()
            )),
            Some((Kind::WrongCall, Rule::Keys))
        );
        // Row counts that differ are refused as the batch is made.
        let two = Keys(&two_keys);
        assert_eq!(
            refusal(Batch::new(
                &hashes[..69],
                &two,
                &mut pending_words,
                &mut refs
            )),
            Some((Kind::WrongCall, Rule::RowCounts))
        );
        assert_eq!(
            refusal(Batch::new(
                &hashes,
                &two,
                &mut pending_words,
                &mut refs[..69]
            )),
            Some((Kind::WrongCall, Rule::MaskPadding))
        );
        let mask = RowMask::try_new(70, &mut pending_words)?;
        assert_eq!(
            refusal(Batch::from_mask(&hashes, &two, mask, &mut refs[..69])),
            Some((Kind::WrongCall, Rule::RowCounts))
        );
        assert_eq!(
            refusal(shared.append(
                chunk,
                Some(&payload[..8]),
                &mut Batch::new(&hashes, &two, &mut pending_words, &mut refs).unwrap()
            )),
            Some((Kind::WrongCall, Rule::PayloadBytes))
        );
        assert_eq!(pending_words, all_rows(70), "no row left the batch");
        assert_eq!(shared.stats().records, 0);
        let mut found_words = rows_words.clone();
        let mut matches = vec![0; 70];
        let mut found = Matches::new(&mut found_words, &mut matches)?;
        assert_eq!(
            refusal(shared.probe(&hashes, &Keys(&one_key), &mut found)),
            Some((Kind::WrongCall, Rule::Keys))
        );
        assert_eq!(
            refusal(shared.probe(&hashes[..69], &Keys(&two_keys), &mut found)),
            Some((Kind::WrongCall, Rule::RowCounts))
        );
        // A mask and references of other lengths are refused as the
        // matches are made.
        assert_eq!(
            refusal(Matches::new(&mut found_words, &mut refs[..69])),
            Some((Kind::WrongCall, Rule::MaskPadding))
        );
        assert_eq!(
            refusal(MatchesView::new(rows, &refs[..69])),
            Some((Kind::WrongCall, Rule::RowCounts))
        );
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
    for (what, damaged, rule) in [
        (
            "a chunk that does not exist",
            reference(3, CHUNK_HEADER),
            Rule::Outside,
        ),
        (
            "a chunk's used mark",
            reference(0, 0) | 1 << UNIT_BITS,
            Rule::Outside,
        ),
        ("past the chunk", reference(0, CHUNK - 8), Rule::Outside),
        ("inside a record", refs[0] + 1, Rule::Misplaced),
    ] {
        set_u32(table.index_words(), bucket, damaged);
        assert_eq!(
            refusal(table_probe_one(&table, &hashes, &keys)),
            Some((Kind::Damaged, rule)),
            "a head at {what}"
        );
        table.index_words()[bucket / 8] = head;
    }
    assert!(table_probe_one(&table, &hashes, &keys)?);
    assert_eq!(
        refusal(table.table()?.record(refs[0] + 1)),
        Some((Kind::Damaged, Rule::Misplaced)),
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
    // walk going around the loop: past a long chain's steps it meets the
    // place it saved, and stops.
    set_u64(table.index_words(), NRECORDS, u64::MAX);
    let result = table_probe_one(&table, &hashes, &absent_keys);
    assert_eq!(refusal(result), Some((Kind::Damaged, Rule::Chain)));

    // A used mark that ends no record, or lies past the chunk.
    for used in [12, CHUNK as u64 + 8] {
        table.chunk_words(0)[0] = used;
        assert_eq!(
            refusal(table.table_mut()?.scan(&mut Cursor::start(), &mut [0; 8])),
            Some((Kind::Damaged, Rule::UsedMark))
        );
        let mut from = CHUNK_HEADER;
        assert_eq!(
            refusal(table.table_mut()?.link(0, &mut from)),
            Some((Kind::Damaged, Rule::UsedMark))
        );
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
    assert_eq!(
        refusal(table.record(0)),
        Some((Kind::WrongCall, Rule::Outside))
    );
    let rows = RowMaskView::try_new(1, &[1])?;
    assert_eq!(
        refusal(table.gather(MatchesView::new(rows, &[0])?, 0, &mut [0])),
        Some((Kind::WrongCall, Rule::Outside))
    );
    assert_eq!(
        refusal(table.next_match(&mut Matches::new(&mut [1], &mut [0])?)),
        Some((Kind::WrongCall, Rule::Outside))
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

/// A cycle of matching records ends a walk over the matches: each step
/// finds a next record, but a lookup counts its steps, and one past the
/// table's count of records fails, by a next match and in a group.
#[test]
fn a_cycle_of_matching_records_ends_a_walk_over_the_matches() -> Result<()> {
    // Key 5 once, key 7 twice: three records.
    let values = [5, 7, 7];
    let keys = [Column::new(&values, None)];
    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
    let mut table = local(&ONE_INT4, 8)?;
    let refs = insert_all(&mut table, &hashes, &Keys(&keys), None)?;
    // The record of 5 names itself, and the two of 7 name each other:
    // every step finds a next record of the same keys.
    for (from, to) in [(0, 0), (1, 2), (2, 1)] {
        let (chunk, byte) = placement(refs[from]);
        set_u32(table.chunk_words(chunk), byte + 4, refs[to]);
    }
    let shared = table.table()?;
    let records = shared.stats().records;
    for in_group in [false, true] {
        for key in [5, 7] {
            let (mut rows, mut found) = ([1], [0]);
            let mut matches = Matches::new(&mut rows, &mut found)?;
            let probe_keys = [Column::new(&[key], None)];
            shared.probe(&[hash_i32(key)], &Keys(&probe_keys), &mut matches)?;
            let mut taken = 0;
            let ended = loop {
                let step = if in_group {
                    shared.next_in_group(&mut matches)
                } else {
                    shared.next_match(&mut matches)
                };
                match step {
                    Ok(1) => taken += 1,
                    other => break other,
                }
                assert!(taken <= records, "the walk ends");
            };
            assert_eq!(
                (refusal(ended), taken),
                (Some((Kind::Damaged, Rule::Chain)), records),
                "key {key}, in a group: {in_group}"
            );
            // A probe starts the count again.
            shared.probe(&[hash_i32(key)], &Keys(&probe_keys), &mut matches)?;
            assert_eq!(matches.steps(), 0);
        }
    }
    Ok(())
}

/// A count of steps a caller gives at its most, as C may keep it between
/// calls, ends a walk over the matches with a cycle, by a next match and
/// in a group, rather than a panic of its sum.
#[test]
fn a_count_of_steps_at_its_most_ends_a_walk_over_the_matches() -> Result<()> {
    let values = [7, 7];
    let keys = [Column::new(&values, None)];
    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
    let mut table = local(&ONE_INT4, 8)?;
    insert_all(&mut table, &hashes, &Keys(&keys), None)?;
    let shared = table.table()?;
    for in_group in [false, true] {
        let (mut rows, mut found) = ([1], [0]);
        let mut matches = Matches::new(&mut rows, &mut found)?;
        let probe_keys = [Column::new(&[7], None)];
        shared.probe(&[hash_i32(7)], &Keys(&probe_keys), &mut matches)?;
        let mut matches = matches.with_steps(u64::MAX);
        let step = if in_group {
            shared.next_in_group(&mut matches)
        } else {
            shared.next_match(&mut matches)
        };
        assert_eq!(
            refusal(step),
            Some((Kind::Damaged, Rule::Chain)),
            "in a group: {in_group}"
        );
    }
    Ok(())
}

/// A healthy chain longer than the 64 steps a walk takes in its loops
/// goes on out of line to its end, and no walk takes it for a cycle: 300
/// records of one hash found row by row and by whole words, an absent key
/// walked past all of them, the 300 matches of one key followed down the
/// chain, and a find or insert of a known key and of a new one.
#[test]
fn a_healthy_chain_longer_than_a_short_walk_is_walked_to_its_end() -> Result<()> {
    let hashes = vec![7_u32; 300];
    let values: Vec<i32> = (0..300).collect();
    let mut table = local(&ONE_INT4, 1024)?;
    let refs = insert_all(
        &mut table,
        &hashes,
        &Keys(&[Column::new(&values, None)]),
        None,
    )?;
    let shared = table.table()?;
    // Row by row, the walk of a key alone: the first key inserted is
    // last in the chain.
    for key in [0_i32, 1, 100, 235, 299] {
        let (found, got) = probe_rows(&shared, &[7], &[Column::new(&[key], None)], &[1])?;
        assert_eq!((found, got[0]), (vec![1], refs[key as usize]), "key {key}");
    }
    let (found, _) = probe_rows(&shared, &[7], &[Column::new(&[1000], None)], &[1])?;
    assert_eq!(found, [0], "an absent key, past 300 records");
    // By whole words, the walk of a word's rows together.
    let (found, got) = probe_rows(
        &shared,
        &hashes[..64],
        &[Column::new(&values[..64], None)],
        &[u64::MAX],
    )?;
    assert_eq!((found, &got[..]), (vec![u64::MAX], &refs[..64]));
    let absent = [1000; 64];
    let (found, _) = probe_rows(
        &shared,
        &hashes[..64],
        &[Column::new(&absent, None)],
        &[u64::MAX],
    )?;
    assert_eq!(found, [0], "a word of absent keys");

    // One key's 300 records: its matches down the chain, as many steps
    // as the table has records, then a find or insert of it and of a new
    // key.
    let same = [5; 300];
    let mut table = local(&ONE_INT4, 1024)?;
    insert_all(
        &mut table,
        &hashes,
        &Keys(&[Column::new(&same, None)]),
        None,
    )?;
    {
        let shared = table.table()?;
        let (mut rows, mut found) = ([1], [0]);
        let mut matches = Matches::new(&mut rows, &mut found)?;
        let mut left = shared.probe(&[7], &Keys(&[Column::new(&[5], None)]), &mut matches)?;
        let mut seen = 0;
        while left > 0 {
            seen += 1;
            left = shared.next_match(&mut matches)?;
        }
        assert_eq!(seen, 300, "the matches of one key");
    }
    for (key, made) in [(1000, 1), (5, 0)] {
        let (mut inserted, mut got) = ([0], [0]);
        let resolved = table.find_or_insert(
            &[7],
            &Keys(&[Column::new(&[key], None)]),
            RowMaskView::try_new(1, &[1])?,
            &mut got,
            &mut RowMask::try_new(1, &mut inserted)?,
        )?;
        assert_eq!((resolved, inserted), (1, [made]), "key {key}");
    }
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
    // As a row alone does, the word stops at the cycle when the count of
    // records is damaged too.
    set_u64(table.index_words(), NRECORDS, u64::MAX);
    let result = probe_rows(&table.table()?, &hashes, &absent_keys, &all_rows(64));
    assert_eq!(refusal(result), Some((Kind::Damaged, Rule::Chain)));
    Ok(())
}

/// A cycle of many records under a count damaged past every bound but the
/// chunks' stops a walk soon, row by row, by whole words and in a find or
/// insert: a walk past a long chain's steps meets the place it saved.
#[test]
fn a_cycle_under_a_damaged_count_stops_a_walk_soon() -> Result<()> {
    // A hundred records of one hash: each new one heads the chain.
    let values: Vec<i32> = (0..100).collect();
    let keys = [Column::new(&values, None)];
    let mut table = local(&ONE_INT4, 100)?;
    let refs = insert_all(&mut table, &[7; 100], &Keys(&keys), None)?;
    // The first record, last in the chain, leads back to its head.
    let (chunk, byte) = placement(refs[0]);
    set_u32(table.chunk_words(chunk), byte + 4, refs[99]);
    set_u64(table.index_words(), NRECORDS, u64::MAX);
    for nrows in [1, 64] {
        let absent = [Column::new(&vec![1000; nrows], None)];
        let result = probe_rows(&table.table()?, &vec![7; nrows], &absent, &all_rows(nrows));
        assert_eq!(
            refusal(result),
            Some((Kind::Damaged, Rule::Chain)),
            "{nrows} rows"
        );
    }
    let absent = [Column::new(&[1000], None)];
    let found = table.table_mut()?.find_or_insert(
        chunk,
        &mut Batch::new(&[7], &Keys(&absent), &mut [1], &mut [0])?,
        &mut RowMask::try_new(1, &mut [0])?,
    );
    assert_eq!(refusal(found), Some((Kind::Damaged, Rule::Chain)));
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
    assert_eq!(
        refusal(table.gather(MatchesView::new(rows, &matches)?, 9, &mut out)),
        Some((Kind::WrongCall, Rule::PayloadWord))
    );
    assert_eq!(
        refusal(table.gather(MatchesView::new(rows, &matches)?, 4, &mut out)),
        Some((Kind::WrongCall, Rule::PayloadWord))
    );
    assert_eq!(
        refusal(table.gather_scattered(MatchesView::new(rows, &matches)?, 4, &mut out)),
        Some((Kind::WrongCall, Rule::PayloadWord))
    );
    assert_eq!(
        refusal(table.gather(MatchesView::new(rows, &matches)?, usize::MAX, &mut out)),
        Some((Kind::WrongCall, Rule::PayloadWord))
    );
    assert_eq!(
        refusal(MatchesView::new(rows, &matches[1..])),
        Some((Kind::WrongCall, Rule::RowCounts))
    );
    assert_eq!(
        refusal(table.gather(MatchesView::new(rows, &matches)?, 0, &mut out[1..])),
        Some((Kind::WrongCall, Rule::RowCounts))
    );
    let mut inside = matches.clone();
    inside[0] += 1;
    assert_eq!(
        refusal(table.gather(MatchesView::new(rows, &inside)?, 0, &mut out)),
        Some((Kind::Damaged, Rule::Misplaced))
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
        table.table_mut()?.append(
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
    assert_eq!(
        refusal(shared.gather_scattered(MatchesView::new(rows, &shuffled)?, 16, &mut out)),
        Some((Kind::WrongCall, Rule::PayloadWord))
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
    assert_eq!(
        refusal(shared.gather_words(MatchesView::new(rows, &shuffled)?, 0, &mut [])),
        Some((Kind::WrongCall, Rule::PayloadWords))
    );
    assert_eq!(
        refusal(shared.gather_words(MatchesView::new(rows, &shuffled)?, 1, &mut [&mut a, &mut b])),
        Some((Kind::WrongCall, Rule::PayloadWords))
    );
    assert_eq!(
        refusal(shared.gather_words(
            MatchesView::new(rows, &shuffled)?,
            0,
            &mut [&mut a, &mut short]
        )),
        Some((Kind::WrongCall, Rule::BatchRows))
    );
    Ok(())
}

/// An append through the table's index refuses keys or a payload that are
/// not the table's, from an array or from columns: it writes nothing and
/// leaves every row pending, and the table's own rows then go in. A chunk
/// writer, which has no index, writes the records of its configuration.
#[test]
fn an_append_checks_the_table_s_index() -> Result<()> {
    let values: Vec<i32> = (1..=4).collect();
    let one = [Column::new(&values, None)];
    let two = [Column::new(&values, None), Column::new(&values, None)];
    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
    // 16 bytes a row, where the table's records hold 8.
    let wide = payload_for(8);
    let column: Vec<u64> = (0..4).collect();
    let nulls = [false; 4];
    let column_values: [&[u64]; 1] = [&column];
    let column_nulls: [&[bool]; 1] = [&nulls];
    // A column takes its word and a word of NULL bits: 16 bytes too.
    let columns = PayloadColumns::new(&column_values, &column_nulls, 4)?;
    let mut table = local(&ONE_INT4, 16)?;
    let chunk = table.add_chunk()?;
    let (one, two) = (Keys(&one), Keys(&two));
    let mut pending = all_rows(4);
    let mut refs = vec![0; 4];
    {
        let mut writer = table.table_mut()?;
        let mut batch = Batch::new(&hashes, &one, &mut pending, &mut refs)?;
        assert_eq!(
            refusal(writer.append(chunk, Some(&wide), &mut batch)),
            Some((Kind::WrongCall, Rule::PayloadBytes))
        );
        assert_eq!(
            refusal(writer.append_columns(chunk, &columns, &mut batch)),
            Some((Kind::WrongCall, Rule::PayloadColumns))
        );
        let mut batch = Batch::new(&hashes, &two, &mut pending, &mut refs)?;
        assert_eq!(
            refusal(writer.append(chunk, None, &mut batch)),
            Some((Kind::WrongCall, Rule::Keys))
        );
    }
    assert_eq!(pending, all_rows(4), "a row left the batch");
    assert_eq!(table.chunk_words(chunk)[0], CHUNK_HEADER as u64, "written");
    let own = payload_for(4);
    let placed = table.table_mut()?.append(
        chunk,
        Some(&own),
        &mut Batch::new(&hashes, &one, &mut pending, &mut refs)?,
    )?;
    assert_eq!((placed, pending[0]), (4, 0));
    let size = record_bytes(&ONE_INT4)?;
    assert_eq!(
        table.chunk_words(chunk)[0],
        (CHUNK_HEADER + 4 * size) as u64
    );

    // Two keys and 16 bytes of payload, without an index.
    let config = TableConfig {
        nkeys: 2,
        payload_size: 16,
    };
    let size = record_bytes(&config)?;
    let mut blocks = Blocks::new();
    let free = blocks.add(CHUNK_HEADER + 4 * size) as usize;
    let mut pending = all_rows(4);
    let placed = writer(&config, blocks.chunks())?.append(
        free,
        Some(&wide),
        &mut Batch::new(&hashes, &two, &mut pending, &mut refs)?,
    )?;
    assert_eq!((placed, pending[0]), (4, 0));
    for (row, &reference) in refs.iter().enumerate() {
        assert_eq!(placement(reference), (free, CHUNK_HEADER + row * size));
    }
    Ok(())
}

/// `gather_key` gives each found row its record's word of a key and its
/// NULL flag, an int4 and an int8 key alike; the rows outside the mask
/// keep their values, and a key past the table's keys is refused before
/// any change.
#[test]
fn gather_key_reads_the_key_of_each_found_row() -> Result<()> {
    let config = TableConfig {
        nkeys: 2,
        payload_size: 8,
    };
    let small: Vec<i32> = (0..70).collect();
    let large: Vec<i64> = (0..70).map(|row| (row << 40) - 3).collect();
    // Every fifth row's first key is NULL.
    let mut non_null = all_rows(70);
    for row in (0..70).step_by(5) {
        non_null[row / 64] &= !(1 << (row % 64));
    }
    let keys = [
        Column::new(&small, Some(&non_null)),
        Column::new(&large, None),
    ];
    let hashes: Vec<u32> = small.iter().map(|&value| hash_i32(value)).collect();
    let mut table = local(&config, 70)?;
    let refs = insert_all(&mut table, &hashes, &Keys(&keys), Some(&payload_for(70)))?;
    // Every other row found.
    let found = [0x5555_5555_5555_5555, 0b10_1010];
    let rows = RowMaskView::try_new(70, &found)?;
    let table = table.table()?;
    let (mut values, mut nulls) = (vec![u64::MAX; 70], vec![true; 70]);
    table.gather_key(MatchesView::new(rows, &refs)?, 0, &mut values, &mut nulls)?;
    for row in 0..70 {
        let null = !selected(&non_null, row);
        let expected = match (selected(&found, row), null) {
            (false, _) => (u64::MAX, true),
            (true, true) => (0, true),
            (true, false) => (row as u64, false),
        };
        assert_eq!((values[row], nulls[row]), expected, "row {row}");
    }
    table.gather_key(MatchesView::new(rows, &refs)?, 1, &mut values, &mut nulls)?;
    for row in (0..70).filter(|&row| selected(&found, row)) {
        assert_eq!((values[row], nulls[row]), (large[row] as u64, false));
    }
    let (before, before_nulls) = (values.clone(), nulls.clone());
    assert_eq!(
        refusal(table.gather_key(MatchesView::new(rows, &refs)?, 2, &mut values, &mut nulls)),
        Some((Kind::WrongCall, Rule::Key))
    );
    assert_eq!((values, nulls), (before, before_nulls));
    Ok(())
}
