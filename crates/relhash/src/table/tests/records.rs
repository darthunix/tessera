//! Records by reference and in order: cursors, marks and walks, the
//! record of a group found or created, a larger index, chains of grouped
//! records, and payload columns.

use anyhow::{Result, ensure};

use super::*;
use crate::table::testing::{Column, Keys, refusal};

/// Marks a test keeps: a run of words per chunk.
struct TestMarks(Vec<Vec<std::cell::Cell<u64>>>);

impl Marks for TestMarks {
    fn set(&self, chunk: usize, word: usize, bit: u64) {
        let cell = &self.0[chunk][word];
        cell.set(cell.get() | bit);
    }

    fn word(&self, chunk: usize, word: usize) -> u64 {
        self.0[chunk][word].get()
    }
}

/// The words of marks a chunk takes, and the one bit a record's mark is:
/// record 70 of chunk 1, of records of 32 bytes, is bit 6 of word 1 of
/// that chunk's run.
#[test]
fn a_record_s_mark_is_its_bit_of_its_chunk_s_run() -> Result<()> {
    for (len, words) in [(8, 0), (40, 1), (2056, 1), (2088, 2)] {
        assert_eq!(mark_words(len, 32)?, words, "a chunk of {len} bytes");
    }
    let len = CHUNK_HEADER + 100 * 32;
    let marks = TestMarks(
        (0..2)
            .map(|_| vec![std::cell::Cell::new(0); mark_words(len, 32).unwrap()])
            .collect(),
    );
    let reference = (1 << UNIT_BITS) | ((CHUNK_HEADER + 70 * 32) as u32 / 8);
    let one = all_rows(1);
    mark(
        &[len, len],
        32,
        &[reference],
        &RowMaskView::try_new(1, &one)?,
        &marks,
    )?;
    let words: Vec<Vec<u64>> = marks
        .0
        .iter()
        .map(|run| run.iter().map(std::cell::Cell::get).collect())
        .collect();
    assert_eq!(words, [vec![0, 0], vec![0, 1 << 6]]);
    Ok(())
}

/// A table marks its records and walks the unmarked ones over its own
/// chunks, with its own record size.
#[test]
fn a_table_marks_and_walks_with_its_own_record_size() -> Result<()> {
    let values: Vec<i32> = (0..10).collect();
    let keys = [Column::new(&values, None)];
    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
    let mut table = local(&ONE_INT4, 10)?;
    let refs = insert_all(&mut table, &hashes, &Keys(&keys), None)?;
    let shared = table.table()?;
    let words_of_marks = mark_words(CHUNK, shared.record_size())?;
    let marks = TestMarks(vec![vec![std::cell::Cell::new(0); words_of_marks]]);
    // Rows 1, 4 and 7 find a pair.
    let paired = words(&(0..10).map(|row| row % 3 == 1).collect::<Vec<_>>());
    shared.mark(&refs, &RowMaskView::try_new(10, &paired)?, &marks)?;
    let mut out = [0; 16];
    let count = shared.scan_unmarked(&mut Cursor::start(), Some(&marks), &mut out)?;
    let unpaired: Vec<u32> = (0..10)
        .filter(|row| row % 3 != 1)
        .map(|row| refs[row])
        .collect();
    assert_eq!(out[..count], unpaired);
    Ok(())
}

/// RIGHT and FULL joins: the records a batch's pairs mark are left out of
/// the walk, which goes on where it stopped whatever the room for its
/// output; without marks it visits every record. The records cross words
/// of marks and chunks of 100 records.
#[test]
fn marked_records_are_left_out_of_the_walk() {
    let record = local(&ONE_INT4, 4).unwrap().table().unwrap().record_size();
    let chunk_bytes = CHUNK_HEADER + 100 * record;
    // Seven chunks at most; three under Miri, which would take minutes.
    let most: usize = if cfg!(miri) { 300 } else { 700 };
    property(
        (1..most, 1..9_u64, 1..70_usize),
        |(nkeys, every, step)| -> Result<()> {
            let values: Vec<i32> = (0..nkeys as i32).collect();
            let keys = [Column::new(&values, None)];
            let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
            let mut table = OwnedTable::new(&ONE_INT4, 2048, chunk_bytes)?;
            resolve_all(&mut table, &hashes, &Keys(&keys))?;
            let all = scan_all(&mut table, 64)?;
            let nchunks = all
                .iter()
                .map(|&offset| offset >> UNIT_BITS)
                .max()
                .unwrap_or(0) as usize
                + 1;
            let marks = TestMarks(
                (0..nchunks)
                    .map(|_| {
                        vec![std::cell::Cell::new(0); mark_words(chunk_bytes, record).unwrap()]
                    })
                    .collect(),
            );
            // Mark one record in `every`, by a batch of all of them.
            let marked: Vec<bool> = (0..all.len())
                .map(|at| (at as u64 * 7 + 3).is_multiple_of(every))
                .collect();
            let nrows = all.len();
            let rows = words(&marked);
            let chunks = table.chunk_set()?;
            mark(
                chunks.lens(),
                record,
                &all,
                &RowMaskView::try_new(nrows, &rows)?,
                &marks,
            )?;
            let walk = |marks: Option<&TestMarks>| -> Result<Vec<u32>> {
                let mut cursor = Cursor::start();
                let mut out = vec![0; step];
                let mut found = Vec::new();
                // A call gives at least a record until the walk is over: a
                // cursor that stood still fails here, not hangs.
                for _ in 0..=all.len() {
                    let count = scan_unmarked(&chunks, record, &mut cursor, marks, &mut out)?;
                    if count == 0 {
                        break;
                    }
                    found.extend_from_slice(&out[..count]);
                }
                ensure!(
                    scan_unmarked(&chunks, record, &mut cursor, marks, &mut out)? == 0,
                    "the walk stays over"
                );
                Ok(found)
            };
            let unmarked: Vec<u32> = all
                .iter()
                .zip(&marked)
                .filter(|(_, marked)| !**marked)
                .map(|(&offset, _)| offset)
                .collect();
            ensure!(
                walk(Some(&marks))? == unmarked,
                "the records without a mark"
            );
            ensure!(walk(None)? == all, "every record without marks");
            Ok(())
        },
    );
}

#[test]
fn a_payload_cursor_changes_payloads_by_reference() -> Result<()> {
    let values: Vec<i32> = (0..100).map(|row| row % 10).collect();
    let keys = [Column::new(&values, None)];
    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
    let mut table = local(&ONE_INT4, 20)?;
    let (refs, _) = resolve_all(&mut table, &hashes, &Keys(&keys))?;
    {
        let mut writer = table.table_mut()?;
        let mut payloads = writer.payloads();
        for (row, &offset) in refs.iter().enumerate() {
            let payload = payloads.payload(offset)?;
            let sum = u64::from_ne_bytes(payload[..8].try_into()?) + row as u64;
            payload[..8].copy_from_slice(&sum.to_ne_bytes());
        }
    }
    // Key k has the rows k, k + 10, ..., k + 90.
    for (key, &offset) in refs[..10].iter().enumerate() {
        let sum = 10 * key as u64 + 450;
        assert_eq!(table.table()?.record(offset)?.payload, sum.to_ne_bytes());
    }

    // The record of the third of five references claims another length:
    // the cursor fails there, and the first two payloads keep what it
    // wrote.
    let (chunk, byte) = placement(refs[2]);
    set_u32(table.chunk_words(chunk), byte + 12, 99);
    let mut opened = Vec::new();
    {
        let mut writer = table.table_mut()?;
        let mut payloads = writer.payloads();
        let failed = refs[..5].iter().try_for_each(|&offset| -> Result<()> {
            payloads
                .payload(offset)?
                .copy_from_slice(&7u64.to_ne_bytes());
            opened.push(offset);
            Ok(())
        });
        assert_eq!(refusal(failed), Some((Kind::Damaged, Rule::Misplaced)));
    }
    assert_eq!(opened, refs[..2]);
    for &offset in &refs[..2] {
        assert_eq!(table.table()?.record(offset)?.payload, 7u64.to_ne_bytes());
    }
    Ok(())
}

#[test]
fn a_payload_cursor_holds_checked_references_to_open_later() -> Result<()> {
    let values: Vec<i32> = (0..40).collect();
    let keys = [Column::new(&values, None)];
    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
    let mut table = local(&ONE_INT4, 40)?;
    let (refs, _) = resolve_all(&mut table, &hashes, &Keys(&keys))?;
    {
        let mut writer = table.table_mut()?;
        let mut payloads = writer.payloads();
        assert_eq!(
            refusal(payloads.held(0)),
            Some((Kind::WrongCall, Rule::EmptySlot)),
            "a slot that holds nothing"
        );
        // Check the first HELD references now, then open them in reverse.
        for (slot, &offset) in refs[..HELD].iter().enumerate() {
            payloads.hold(slot, offset)?;
        }
        for slot in (0..HELD).rev() {
            payloads
                .held(slot)?
                .copy_from_slice(&(slot as u64 + 100).to_ne_bytes());
        }
        assert_eq!(
            refusal(payloads.hold(HELD, refs[0])),
            Some((Kind::WrongCall, Rule::Slot)),
            "past the slots"
        );
        assert_eq!(
            refusal(payloads.held(HELD)),
            Some((Kind::WrongCall, Rule::EmptySlot)),
            "past the slots"
        );
        assert_eq!(
            refusal(payloads.hold(0, refs[0] + 1)),
            Some((Kind::Damaged, Rule::Misplaced)),
            "inside a record"
        );
        // A slot that took a new reference opens that one.
        payloads.hold(0, refs[HELD])?;
        payloads.held(0)?.copy_from_slice(&7u64.to_ne_bytes());
    }
    for (slot, &offset) in refs[..HELD].iter().enumerate().skip(1) {
        let expected = slot as u64 + 100;
        assert_eq!(
            table.table()?.record(offset)?.payload,
            expected.to_ne_bytes()
        );
    }
    assert_eq!(
        table.table()?.record(refs[0])?.payload,
        100u64.to_ne_bytes()
    );
    assert_eq!(
        table.table()?.record(refs[HELD])?.payload,
        7u64.to_ne_bytes()
    );
    Ok(())
}

#[test]
fn a_record_cursor_reads_records_by_reference() -> Result<()> {
    let values: Vec<i32> = (0..100).map(|row| row % 10).collect();
    let keys = [Column::new(&values, None)];
    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
    let mut table = local(&ONE_INT4, 20)?;
    let (refs, _) = resolve_all(&mut table, &hashes, &Keys(&keys))?;
    let reader = table.table()?;
    let mut records = reader.records();
    for (row, &offset) in refs.iter().enumerate() {
        let record = records.record(offset)?;
        assert_eq!(record.keys, &[(row % 10) as i64]);
        assert_eq!(record.hash, hashes[row]);
    }
    assert_eq!(
        refusal(records.record(refs[0] + 1)),
        Some((Kind::Damaged, Rule::Misplaced)),
        "inside a record"
    );
    assert_eq!(
        refusal(records.record(0)),
        Some((Kind::WrongCall, Rule::Outside)),
        "reference 0"
    );
    // The records it gave outlive the next read.
    let first = records.record(refs[0])?;
    let second = records.record(refs[1])?;
    assert_ne!(first.keys, second.keys);
    Ok(())
}

#[test]
fn each_payload_hands_the_selected_rows_payloads_in_turn() -> Result<()> {
    let values: Vec<i32> = (0..100).map(|row| row % 10).collect();
    let keys = [Column::new(&values, None)];
    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
    let mut table = local(&ONE_INT4, 20)?;
    let (mut refs, _) = resolve_all(&mut table, &hashes, &Keys(&keys))?;
    // Rows 3 and 13 share a record, row 64 is in the second word.
    let words = [1u64 << 3 | 1 << 13 | 1 << 40, 1 << 0 | 1 << 35];
    let rows = RowMaskView::try_new(100, &words)?;
    let mut seen = Vec::new();
    table
        .table_mut()?
        .each_payload(&refs, &rows, |row, payload| {
            let count = u64::from_ne_bytes(payload[..8].try_into().unwrap());
            payload[..8].copy_from_slice(&(count + 1).to_ne_bytes());
            seen.push(row);
        })?;
    assert_eq!(seen, [3, 13, 40, 64, 99], "the selected rows, in order");
    for (key, &offset) in refs[..10].iter().enumerate() {
        let payload = table.table()?.record(offset)?.payload.to_vec();
        let expected: u64 = match key {
            3 => 2,
            0 | 4 | 9 => 1,
            _ => 0,
        };
        assert_eq!(payload, expected.to_ne_bytes(), "key {key}");
    }

    // A reference that names no record fails, after the rows before it.
    refs[40] += 1;
    seen.clear();
    let failed = table
        .table_mut()?
        .each_payload(&refs, &rows, |row, _| seen.push(row));
    assert_eq!(refusal(failed), Some((Kind::Damaged, Rule::Misplaced)));
    assert_eq!(seen, [3, 13]);
    // Offsets for another count of rows than the mask's fail first.
    seen.clear();
    let short = table
        .table_mut()?
        .each_payload(&refs[..99], &rows, |row, _| seen.push(row));
    assert_eq!(refusal(short), Some((Kind::WrongCall, Rule::RowCounts)));
    assert!(seen.is_empty());
    Ok(())
}

/// The pointers to the payloads of a batch: rows of one record get one
/// pointer, and each changes the payload after the call, beside the
/// others, which Miri checks among the table's tests. A reference that
/// names no record fails as for `each_payload`.
#[test]
fn pointers_to_the_payload_of_one_record_write_it_in_turn() -> Result<()> {
    let values: Vec<i32> = (0..100).map(|row| row % 10).collect();
    let keys = [Column::new(&values, None)];
    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
    let mut table = local(&ONE_INT4, 20)?;
    let (mut refs, _) = resolve_all(&mut table, &hashes, &Keys(&keys))?;
    let words = all_rows(100);
    let rows = RowMaskView::try_new(100, &words)?;
    let mut pointers = vec![core::ptr::null_mut(); 100];
    table
        .table_mut()?
        .payload_ptrs(&refs, &rows, &mut pointers)?;
    assert_eq!(pointers[3], pointers[13], "one record, one pointer");
    assert_ne!(pointers[3], pointers[4]);
    for &pointer in &pointers {
        // SAFETY: each pointer is a record's payload of 8 bytes, aligned to
        // 8, which nothing borrows while the loop writes it.
        unsafe {
            let count = pointer.cast::<u64>().read();
            pointer.cast::<u64>().write(count + 1);
        }
    }
    for (key, &offset) in refs[..10].iter().enumerate() {
        let payload = table.table()?.record(offset)?.payload.to_vec();
        assert_eq!(payload, 10u64.to_ne_bytes(), "key {key}");
    }

    refs[40] += 1;
    pointers.fill(core::ptr::null_mut());
    let failed = table.table_mut()?.payload_ptrs(&refs, &rows, &mut pointers);
    assert_eq!(refusal(failed), Some((Kind::Damaged, Rule::Misplaced)));
    assert!(pointers[..40].iter().all(|pointer| !pointer.is_null()));
    assert!(pointers[40..].iter().all(|pointer| pointer.is_null()));
    let short = table
        .table_mut()?
        .payload_ptrs(&refs, &rows, &mut pointers[..99]);
    assert_eq!(refusal(short), Some((Kind::WrongCall, Rule::RowCounts)));
    Ok(())
}

/// The batch that `Batch`'s documentation draws: four rows, row 2 left
/// out as a NULL key would be, into chunks of two records each.
#[test]
fn a_batch_goes_through_two_chunks() -> Result<()> {
    let values = [10, 11, 12, 13];
    let keys = [Column::new(&values, None)];
    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
    let mut table = OwnedTable::new(&ONE_INT4, 4, CHUNK_HEADER + 2 * 32)?;
    let (first, second) = (table.add_chunk()?, table.add_chunk()?);
    let mut shared = table.table_mut()?;
    let mut pending = [0b1011];
    let mut refs = [u32::MAX; 4];
    let source = Keys(&keys);
    let mut batch = Batch::new(&hashes, &source, &mut pending, &mut refs)?;
    assert_eq!(shared.append(first, None, &mut batch)?, 2);
    assert_eq!(rows_of(&batch.pending()), [3], "the chunk is full");
    assert_eq!(shared.append(second, None, &mut batch)?, 1);
    assert_eq!(pending, [0], "the batch is done");
    let places = [
        (first, CHUNK_HEADER),
        (first, CHUNK_HEADER + 32),
        (second, CHUNK_HEADER),
    ];
    for (row, place) in [0, 1, 3].into_iter().zip(places) {
        assert_eq!(placement(refs[row]), place, "row {row}");
        assert_eq!(shared.record(refs[row])?.keys, [i64::from(values[row])]);
    }
    assert_eq!(refs[2], u32::MAX, "the row left out keeps its place");
    Ok(())
}

/// The owning table's calls read the rows to place and place them all,
/// over more records than a chunk holds and, for a grouping, more than
/// its first index takes.
#[test]
fn the_owning_table_places_every_row_of_its_mask() -> Result<()> {
    let values: Vec<i32> = (0..100).collect();
    let keys = [Column::new(&values, None)];
    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
    // Every row but each seventh.
    let rows = words(&(0..100).map(|row| row % 7 != 0).collect::<Vec<_>>());
    let placed: Vec<usize> = (0..100).filter(|row| row % 7 != 0).collect();
    let mask = RowMaskView::try_new(100, &rows)?;
    for grouping in [false, true] {
        let mut table = OwnedTable::new(&ONE_INT4, 4, CHUNK_HEADER + 16 * 32)?;
        let mut refs = vec![0; 100];
        let count = if grouping {
            let mut inserted = vec![0; 2];
            table.find_or_insert(
                &hashes,
                &Keys(&keys),
                mask,
                &mut refs,
                &mut RowMask::try_new(100, &mut inserted)?,
            )?
        } else {
            table.insert(&hashes, &Keys(&keys), mask, None, &mut refs)?
        };
        assert_eq!(count, placed.len());
        assert!(table.chunks() > 1, "the records span chunks");
        let shared = table.table()?;
        for row in 0..100 {
            if row % 7 == 0 {
                assert_eq!(refs[row], 0, "row {row} is not placed");
            } else {
                let record = shared.record(refs[row])?;
                assert_eq!(record.keys, [i64::from(values[row])], "row {row}");
            }
        }
    }
    assert_eq!(rows_of(&mask), placed, "the rows stay as they were");
    Ok(())
}

/// Rows 0 and 1 of key 5, rows 2 and 3 NULL, whose slots the source
/// leaves holding 7 and 8.
#[cfg(debug_assertions)]
struct NullSlotsHeld;

#[cfg(debug_assertions)]
impl KeySource for NullSlotsHeld {
    fn nkeys(&self) -> usize {
        1
    }

    fn nrows(&self) -> usize {
        4
    }

    fn word(&self, _: usize, _: usize, selected: u64, out: &mut [i64; 64]) -> Option<u64> {
        for bit in crate::ones(selected) {
            out[bit] = [5, 5, 7, 8][bit];
        }
        Some(selected & 0b0011)
    }
}

/// A source must give 0 in a NULL key's slot: a build with debug
/// assertions stops a source that leaves a value there.
#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "a NULL key's slot holds a value")]
fn a_value_a_source_leaves_in_a_null_slot_is_caught() {
    let hashes = [
        hash_i32(5),
        hash_i32(5),
        hash_i32(GROUP_KEY),
        hash_i32(GROUP_KEY),
    ];
    let mut table = local(&ONE_INT4, 4).unwrap();
    let mut refs = [0; 4];
    let mut inserted = [0];
    let _ = table.find_or_insert(
        &hashes,
        &NullSlotsHeld,
        RowMaskView::try_new(4, &[0b1111]).unwrap(),
        &mut refs,
        &mut RowMask::try_new(4, &mut inserted).unwrap(),
    );
}

#[test]
fn rows_find_or_create_the_record_of_their_keys() -> Result<()> {
    let values: Vec<i32> = (0..100).map(|row| row % 10).collect();
    let keys = [Column::new(&values, None)];
    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
    let mut table = local(&ONE_INT4, 20)?;
    let (refs, created) = resolve_all(&mut table, &hashes, &Keys(&keys))?;
    assert_eq!(
        created,
        (0..10).collect::<Vec<_>>(),
        "the first row of each key"
    );
    assert_eq!(table.table()?.stats().records, 10);
    for row in 0..100 {
        assert_eq!(refs[row], refs[row % 10]);
        assert_eq!(table.table()?.record(refs[row])?.keys, &[(row % 10) as i64]);
        assert_eq!(table.table()?.record(refs[row])?.payload, [0; 8]);
    }
    {
        let mut writer = table.table_mut()?;
        for (key, &offset) in refs[..10].iter().enumerate() {
            writer
                .payload_mut(offset)?
                .copy_from_slice(&(key as u64 * 7).to_ne_bytes());
        }
        assert_eq!(
            refusal(writer.payload_mut(refs[0] + 1)),
            Some((Kind::Damaged, Rule::Misplaced))
        );
    }
    for (row, &offset) in refs.iter().enumerate() {
        let payload = table.table()?.record(offset)?.payload.to_vec();
        assert_eq!(payload, ((row % 10) as u64 * 7).to_ne_bytes());
    }

    let more: Vec<i32> = (5..15).collect();
    let more_keys = [Column::new(&more, None)];
    let more_hashes: Vec<u32> = more.iter().map(|&value| hash_i32(value)).collect();
    let (more_refs, created) = resolve_all(&mut table, &more_hashes, &Keys(&more_keys))?;
    assert_eq!(
        created,
        (5..10).collect::<Vec<_>>(),
        "keys 10 to 14 are new"
    );
    assert_eq!(more_refs[..5], refs[5..10]);
    let table = table.table()?;
    assert_eq!(table.stats().records, 15);
    let (found, matches) = probe_all(&table, &more_hashes, &Keys(&more_keys))?;
    assert_eq!(found.len(), 10);
    assert_eq!(matches, more_refs);
    Ok(())
}

/// The calls of one batch add the rows whose record they made to one mask
/// of inserted rows, which the caller clears once, and leave its other
/// bits as they were. The pending rows are another thing: a row that
/// finds its record leaves them too, but made none.
#[test]
fn inserted_rows_add_up_over_the_calls_of_a_batch() -> Result<()> {
    // Keys 0 to 5 twice, key 0 in the table before the batch, row 11 left
    // out, into chunks of two records.
    let values: Vec<i32> = (0..12).map(|row| row % 6).collect();
    let keys = [Column::new(&values, None)];
    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
    let mut table = OwnedTable::new(&ONE_INT4, 16, CHUNK_HEADER + 2 * 32)?;
    let (known, _) = resolve_all(
        &mut table,
        &hashes[..1],
        &Keys(&[Column::new(&values[..1], None)]),
    )?;
    let mut pending = [(1 << 11) - 1];
    let mut refs = vec![0; 12];
    // Row 11's bit, set before the batch, is not the calls' to clear.
    let mut inserted_words = [1 << 11];
    let source = Keys(&keys);
    let mut batch = Batch::new(&hashes, &source, &mut pending, &mut refs)?;
    let mut inserted = RowMask::try_new(12, &mut inserted_words)?;
    let mut calls = 0;
    while batch.pending().selected_count() > 0 {
        let chunk = table.add_chunk()?;
        table
            .table_mut()?
            .find_or_insert(chunk, &mut batch, &mut inserted)?;
        calls += 1;
    }
    assert_eq!(calls, 3, "two new records a chunk: rows 1, 2; 3, 4; 5");
    assert_eq!(pending, [0], "every row found or made its record");
    assert_eq!(
        inserted_words,
        [0b11_1110 | (1 << 11)],
        "rows 1 to 5 made one"
    );
    assert_eq!(refs[0], known[0], "row 0 found the record of key 0");
    assert_eq!(refs[6..11], refs[..5], "rows 6 to 10 found those of 0 to 4");
    Ok(())
}

#[test]
fn a_full_chunk_resolves_known_keys_only() -> Result<()> {
    let values: Vec<i32> = (0..20).collect();
    let keys = [Column::new(&values, None)];
    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
    // A chunk of four records.
    let mut table = OwnedTable::new(&ONE_INT4, 20, CHUNK_HEADER + 4 * 32)?;
    let chunk = table.add_chunk()?;
    let mut pending_words = all_rows(20);
    let mut refs = vec![0; 20];
    let mut inserted_words = [0];
    {
        let mut writer = table.table_mut()?;
        let source = Keys(&keys);
        let mut batch = Batch::new(&hashes, &source, &mut pending_words, &mut refs)?;
        let mut inserted = RowMask::try_new(20, &mut inserted_words)?;
        let resolved = writer.find_or_insert(chunk, &mut batch, &mut inserted)?;
        assert_eq!(resolved, 4);
        assert_eq!(rows_of(&batch.pending()), (4..20).collect::<Vec<_>>());
    }
    assert_eq!(inserted_words, [0b1111]);

    let known: Vec<i32> = [3, 2, 1, 0, 3].into();
    let known_keys = [Column::new(&known, None)];
    let known_hashes: Vec<u32> = known.iter().map(|&value| hash_i32(value)).collect();
    let mut known_pending = all_rows(5);
    let mut known_refs = vec![0; 5];
    let mut created = [0];
    let resolved = table.table_mut()?.find_or_insert(
        chunk,
        &mut Batch::new(
            &known_hashes,
            &Keys(&known_keys),
            &mut known_pending,
            &mut known_refs,
        )?,
        &mut RowMask::try_new(5, &mut created)?,
    )?;
    assert_eq!(resolved, 5, "no room is needed for known keys");
    assert_eq!(created, [0]);
    assert_eq!(known_refs[0], refs[3]);
    assert_eq!(known_refs[4], refs[3]);
    assert_eq!(known_refs[3], refs[0]);
    assert_eq!(table.table()?.stats().records, 4);
    Ok(())
}

#[test]
fn an_index_takes_records_up_to_half_its_buckets_and_regrows() -> Result<()> {
    let values: Vec<i32> = (0..1500).collect();
    let keys = [Column::new(&values, None)];
    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
    let mut table = OwnedTable::new(&ONE_INT4, 1, MAX_CHUNK_LEN)?;
    let chunk = table.add_chunk()?;
    let mut pending_words = all_rows(1500);
    let mut refs = vec![0; 1500];
    let mut inserted_words = all_rows(1500);
    let resolved = table.table_mut()?.find_or_insert(
        chunk,
        &mut Batch::new(&hashes, &Keys(&keys), &mut pending_words, &mut refs)?,
        &mut RowMask::try_new(1500, &mut inserted_words)?,
    )?;
    assert_eq!(resolved, 512, "1024 buckets take 512 records");
    // The owner builds larger indexes as it goes; references stay.
    let first = refs[..512].to_vec();
    let (all, created) = resolve_all(&mut table, &hashes, &Keys(&keys))?;
    assert_eq!(all[..512], first);
    assert_eq!(created, (512..1500).collect::<Vec<_>>());
    assert_eq!(table.chunks(), 1, "the records never moved");
    let shared = table.table()?;
    assert_eq!(shared.stats().records, 1500);
    assert!(shared.stats().buckets >= 4096);
    let (found, matches) = probe_all(&shared, &hashes, &Keys(&keys))?;
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
    let mut refs = insert_all(
        &mut table,
        &hashes[..120],
        &Keys(&[Column::new(&values[..120], None)]),
        None,
    )?;
    refs.extend(insert_all(
        &mut table,
        &hashes[120..],
        &Keys(&[Column::new(&values[120..], None)]),
        None,
    )?);
    assert_eq!(table.chunks(), 4);
    for step in [1, 7, 64, 500] {
        assert_eq!(scan_all(&mut table, step)?, refs, "walks of {step}");
    }
    let writer = table.table_mut()?;
    for (what, raw) in [
        ("inside a record", CHUNK_HEADER as u64 + 4),
        ("before the first record", 0),
        ("past the chunks", (6u64 << 32) | CHUNK_HEADER as u64),
    ] {
        let mut cursor = Cursor::from_raw(raw);
        assert_eq!(
            refusal(writer.scan(&mut cursor, &mut [0; 8])),
            Some((Kind::WrongCall, Rule::Cursor)),
            "a cursor {what}"
        );
    }
    // A cursor on a record's place past its chunk's used mark: the walk
    // would skip the rest of the chunk.
    let (chunk, byte) = placement(*refs.last().unwrap());
    let past = byte + 2 * writer.layout.record_size;
    let mut cursor = Cursor::from_raw(((chunk as u64) << 32) | past as u64);
    assert_eq!(
        refusal(writer.scan(&mut cursor, &mut [0; 8])),
        Some((Kind::WrongCall, Rule::CursorPast)),
        "a cursor past the used mark"
    );
    // Nowhere to put records: a count of 0 would read as the end.
    let mut cursor = Cursor::start();
    assert_eq!(
        refusal(writer.scan(&mut cursor, &mut [])),
        Some((Kind::WrongCall, Rule::EmptyWalk)),
        "nowhere to put records"
    );
    assert_eq!(cursor, Cursor::start());
    Ok(())
}

#[test]
fn regrowing_keeps_records_and_their_references() -> Result<()> {
    let values: Vec<i32> = (0..100).map(|row| row % 30).collect();
    let keys = [Column::new(&values, None)];
    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
    let payload = payload_for(100);
    let mut table = local(&ONE_INT4, 16)?;
    let refs = insert_all(&mut table, &hashes, &Keys(&keys), Some(&payload))?;
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
    assert_eq!(before, refs);
    let shared = table.table()?;
    assert_eq!(shared.stats().records, 100);
    let (found, matches) = probe_all(&shared, &hashes, &Keys(&keys))?;
    assert_eq!(found.len(), 100);
    for row in 0..100 {
        let record = shared.record(matches[row])?;
        assert_eq!(record.keys, &[(row % 30) as i64]);
        assert_eq!(
            shared.record(refs[row])?.payload,
            (row as u64 * 10).to_ne_bytes()
        );
    }
    let mut current = matches[..30].to_vec();
    let mut chain_lengths = vec![1; 30];
    let mut rows = all_rows(30);
    let mut more = Matches::new(&mut rows, &mut current)?;
    while shared.next_match(&mut more)? > 0 {
        for key in more.rows().selected_rows() {
            chain_lengths[key] += 1;
        }
    }
    assert_eq!(
        chain_lengths,
        vec![4; 10]
            .into_iter()
            .chain(vec![3; 20])
            .collect::<Vec<_>>()
    );
    assert_eq!(
        refusal(table.regrow(16)),
        Some((Kind::WrongCall, Rule::IndexShrinks)),
        "no shrinking"
    );
    assert_eq!(table.table()?.stats().records, 100);
    Ok(())
}

/// Append rows and link them grouped: the count of rows whose keys the
/// table held already.
fn insert_grouped_rows(
    table: &mut OwnedTable,
    hashes: &[u32],
    keys: &[Column],
    payload: &[u8],
    rows: &[usize],
    refs: &mut [u32],
) -> Result<usize> {
    let nrows = hashes.len();
    let mut selected = vec![0; nrows.div_ceil(64)];
    for &row in rows {
        selected[row / 64] |= 1 << (row % 64);
    }
    let (inserted, duplicates) = table.insert_grouped(
        hashes,
        &Keys(keys),
        RowMaskView::try_new(nrows, &selected)?,
        Some(payload),
        refs,
    )?;
    assert_eq!(inserted, rows.len());
    Ok(duplicates)
}

#[test]
fn grouped_records_of_a_key_lie_together() -> Result<()> {
    // Ten keys, ten rows each, all in one bucket: one hash for every row,
    // so each key's group lies among the other keys' records.
    let values: Vec<i32> = (0..100).map(|row| (row * 7) % 10).collect();
    let keys = [Column::new(&values, None)];
    let hashes = vec![0x1234_5678; 100];
    let payload = payload_for(100);
    let mut table = local(&ONE_INT4, 100)?;
    let mut refs = vec![0; 100];
    let first: Vec<usize> = (0..30).collect();
    let second: Vec<usize> = (30..100).collect();
    let duplicates = insert_grouped_rows(&mut table, &hashes, &keys, &payload, &first, &mut refs)?;
    assert_eq!(duplicates, 20, "a key's first row is new");
    let duplicates = insert_grouped_rows(&mut table, &hashes, &keys, &payload, &second, &mut refs)?;
    assert_eq!(duplicates, 70, "every key was there already");

    // From the probe's record, the group gives every row of the key, one
    // step each, and ends where the next key begins.
    let table = table.table()?;
    let (found, matches) = probe_all(&table, &hashes, &Keys(&keys))?;
    assert_eq!(found.len(), 100);
    let mut seen = [0; 10];
    for key in 0..10 {
        let mut offset = matches[key];
        loop {
            let record = table.record(offset)?;
            let row = u64::from_ne_bytes(record.payload.try_into().unwrap()) / 10;
            assert_eq!(values[row as usize], values[key]);
            seen[values[key] as usize] += 1;
            let mut refs = [offset];
            if table.next_in_group(&mut Matches::new(&mut [1], &mut refs)?)? == 0 {
                break;
            }
            offset = refs[0];
        }
    }
    assert_eq!(seen, [10; 10], "each group holds the ten rows of its key");
    Ok(())
}

#[test]
fn grouped_chains_span_chunks_and_survive_a_larger_index() -> Result<()> {
    let values: Vec<i32> = (0..400).map(|row| row % 7).collect();
    let keys = [Column::new(&values, None)];
    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
    let payload = payload_for(400);
    let mut table = local(&ONE_INT4, 64)?;
    let mut refs = vec![0; 400];
    let rows: Vec<usize> = (0..400).collect();
    let duplicates = insert_grouped_rows(&mut table, &hashes, &keys, &payload, &rows, &mut refs)?;
    assert_eq!(duplicates, 393, "the first row of each key is new");
    assert!(table.chunks() > 3);
    table.regrow(1000)?;

    // next_match walks the chain and finds the same next records, and
    // each key has as many as rows.
    let table = table.table()?;
    let (_, matches) = probe_all(
        &table,
        &hashes[..7],
        &Keys(&[Column::new(&values[..7], None)]),
    )?;
    for (key, &first) in matches.iter().enumerate() {
        let mut by_group = vec![first];
        let mut by_chain = vec![first];
        let mut records = 1;
        loop {
            let mut group_words = [1];
            let mut chain_words = [1];
            table.next_in_group(&mut Matches::new(&mut group_words, &mut by_group)?)?;
            table.next_match(&mut Matches::new(&mut chain_words, &mut by_chain)?)?;
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

/// Records appended from payload columns hold what records appended from
/// the same payload laid out row by row hold: a word of the row's NULL
/// bits per 64 columns, then a word per column, 0 for a NULL; the rows
/// left out of the mask get none, the call allocates no memory, and a
/// column of the wrong length or a payload of the wrong size is refused.
#[test]
fn payload_columns_append_the_records_a_payload_array_does() -> Result<()> {
    // Up to two words of NULL bits under Miri, which would take minutes
    // over more.
    let counts: &[usize] = if cfg!(miri) {
        &[3, 64, 65]
    } else {
        &[3, 64, 65, 130, MAX_PAYLOAD_COLUMNS]
    };
    for &ncolumns in counts {
        payload_columns_of(ncolumns)?;
    }
    Ok(())
}

fn payload_columns_of(ncolumns: usize) -> Result<()> {
    // Two words of rows; under Miri, which would take minutes, a few rows
    // past the first.
    const ROWS: usize = if cfg!(miri) { 70 } else { 150 };
    let null_words = payload_null_words(ncolumns);
    let keys_values: Vec<i32> = (0..ROWS as i32).map(|row| row * 7 % 50).collect();
    let keys = [Column::new(&keys_values, None)];
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
        nkeys: 1,
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
    // Four records a chunk at least.
    let chunk_bytes = CHUNK.max(CHUNK_HEADER + 4 * record_bytes(&config)?);
    for from_columns in [false, true] {
        let mut table = OwnedTable::new(&config, 0, chunk_bytes)?;
        let mut pending_words = selected.clone();
        let mut refs = vec![0u32; ROWS];
        loop {
            let chunk = match table.chunks() {
                0 => table.add_chunk()?,
                n => n - 1,
            };
            let mut shared = table.table_mut()?;
            let source = Keys(&keys);
            let mut batch = Batch::new(&hashes, &source, &mut pending_words, &mut refs)?;
            if from_columns {
                let before = allocations();
                shared.append_columns(chunk, &columns, &mut batch)?;
                assert_eq!(allocations(), before, "{ncolumns} columns allocated");
            } else {
                shared.append(chunk, Some(&payload_bytes), &mut batch)?;
            }
            if pending_words.iter().all(|&word| word == 0) {
                break;
            }
            table.add_chunk()?;
        }
        tables.push((table, refs));
    }
    let (by_rows, refs) = &tables[0];
    let (by_columns, column_refs) = &tables[1];
    assert_eq!(refs, column_refs, "the records lie in the same places");
    let (rows_table, columns_table) = (by_rows.table()?, by_columns.table()?);
    for row in (0..ROWS).filter(|row| row % 3 != 0) {
        let expected = rows_table.record(refs[row])?;
        let got = columns_table.record(column_refs[row])?;
        assert_eq!(got.payload, expected.payload, "row {row}");
        assert_eq!(got.keys, expected.keys);
        assert_eq!(got.hash, expected.hash);
    }
    // Refusals: a column shorter than the batch, a payload of another size.
    let short: Vec<&[u64]> = vec![&values[0][..ROWS - 1]; ncolumns];
    assert_eq!(
        refusal(PayloadColumns::new(&short, &null_slices, ROWS)),
        Some((Kind::WrongCall, Rule::PayloadRows))
    );
    let two = PayloadColumns::new(&value_slices[..2], &null_slices[..2], ROWS)?;
    let mut table = OwnedTable::new(&config, 0, chunk_bytes)?;
    let chunk = table.add_chunk()?;
    let mut pending_words = selected.clone();
    let mut refs = vec![0u32; ROWS];
    assert_eq!(
        refusal(table.table_mut()?.append_columns(
            chunk,
            &two,
            &mut Batch::new(&hashes, &Keys(&keys), &mut pending_words, &mut refs).unwrap()
        )),
        Some((Kind::WrongCall, Rule::PayloadColumns))
    );
    Ok(())
}

/// A larger index, a cleared key and the filters, given chunks of which
/// the last cannot be one, fail with the rule of a chunk before they
/// change anything: the index, the chunks, the filter's words and the
/// shared filter's state are as they were.
#[test]
fn a_wrong_last_chunk_fails_the_calls_over_every_chunk_before_a_change() -> Result<()> {
    let values: Vec<i32> = (0..40).collect();
    let keys = [Column::new(&values, None)];
    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
    let mut table = local(&ONE_INT4, 64)?;
    insert_all(&mut table, &hashes, &Keys(&keys), None)?;
    let index = table.index_words().to_vec();
    let chunks: Vec<Vec<u64>> = (0..table.chunks())
        .map(|chunk| table.chunk_words(chunk).to_vec())
        .collect();
    let (index_base, index_len) = {
        let words = table.index_words();
        (words.as_mut_ptr().cast::<u8>(), words.len() * 8)
    };
    let (mut bases, mut lens) = {
        let set = table.chunk_set()?;
        (set.bases.to_vec(), set.lens.to_vec())
    };
    // A last chunk whose length is no multiple of 8.
    let mut odd = vec![0_u64; 8];
    bases.push(odd.as_mut_ptr().cast());
    lens.push(60);
    let mut grown = vec![0_u64; index_size(&ONE_INT4, 256)? / 8];
    let mut words = [u64::MAX; 8];
    let mut shared = [0_u64; 9];
    {
        // SAFETY: the owner's blocks and the vectors lie apart and live
        // through this block, and nothing else uses them meanwhile.
        let set = unsafe { Chunks::new(&bases, &lens) }?;
        // SAFETY: as above, for the owner's index.
        let mut over = unsafe { TableMut::attach_mut(index_base, index_len, set) }?;
        let wrong = Some((Kind::WrongCall, Rule::Chunk));
        // SAFETY: the new index is a vector of its own, alive meanwhile.
        let regrown = unsafe { over.regrow(grown.as_mut_ptr().cast(), grown.len() * 8, 256) };
        assert_eq!(refusal(regrown), wrong, "a larger index");
        assert_eq!(refusal(over.clear_key(0)), wrong, "a cleared key");
        assert_eq!(refusal(over.bloom(&mut words)), wrong, "a filter");
        let filter = bloom::SharedFilter::from_mut(&mut shared)?;
        assert_eq!(
            refusal(over.try_build_bloom(&filter)),
            wrong,
            "a shared one"
        );
    }
    assert_eq!(words, [u64::MAX; 8], "the filter's words");
    assert_eq!(shared[0], 0, "the shared filter, unclaimed");
    assert_eq!(table.index_words(), &index[..], "the index");
    for (chunk, words) in chunks.iter().enumerate() {
        assert_eq!(table.chunk_words(chunk), &words[..], "chunk {chunk}");
    }
    Ok(())
}

/// Marks that participants share: each word changes by an atomic OR.
struct SharedMarks(Vec<Vec<std::sync::atomic::AtomicU64>>);

impl Marks for SharedMarks {
    fn set(&self, chunk: usize, word: usize, bit: u64) {
        self.0[chunk][word].fetch_or(bit, std::sync::atomic::Ordering::Relaxed);
    }

    fn word(&self, chunk: usize, word: usize) -> u64 {
        self.0[chunk][word].load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Four threads mark the records of their stripes of one batch at once,
/// in marks they share, over records that cross words and chunks; once
/// all are done, the walk gives exactly the records without a mark.
#[test]
fn records_marked_by_threads_at_once_are_left_out_of_the_walk() -> Result<()> {
    let record = record_bytes(&ONE_INT4)?;
    let chunk_bytes = CHUNK_HEADER + 100 * record;
    let values: Vec<i32> = (0..300).collect();
    let keys = [Column::new(&values, None)];
    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
    let mut table = OwnedTable::new(&ONE_INT4, 300, chunk_bytes)?;
    let refs = insert_all(&mut table, &hashes, &Keys(&keys), None)?;
    let chunks = table.chunk_set()?;
    let marks = SharedMarks(
        (0..chunks.len())
            .map(|_| {
                (0..mark_words(chunk_bytes, record).unwrap())
                    .map(|_| std::sync::atomic::AtomicU64::new(0))
                    .collect()
            })
            .collect(),
    );
    // Rows 0, 2, 4, ... find a pair; thread t marks those of rows t mod 4.
    let lens = chunks.lens();
    std::thread::scope(|scope| {
        for thread in 0..4 {
            let (refs, marks) = (&refs, &marks);
            scope.spawn(move || {
                let paired: Vec<bool> = (0..300)
                    .map(|row| row % 4 == thread && row % 2 == 0)
                    .collect();
                let rows = words(&paired);
                mark(
                    lens,
                    record,
                    refs,
                    &RowMaskView::try_new(300, &rows).unwrap(),
                    marks,
                )
                .unwrap();
            });
        }
    });
    let mut cursor = Cursor::start();
    let mut out = [0; 64];
    let mut walked = Vec::new();
    loop {
        let count = scan_unmarked(&chunks, record, &mut cursor, Some(&marks), &mut out)?;
        if count == 0 {
            break;
        }
        walked.extend_from_slice(&out[..count]);
    }
    let unpaired: Vec<u32> = (0..300)
        .filter(|row| row % 2 == 1)
        .map(|row| refs[row])
        .collect();
    assert_eq!(walked, unpaired);
    Ok(())
}

/// A mark of a reference that names no record of its size, a mark or a
/// walk of a size no record has, a walk from a cursor off a record or
/// past its chunk's used mark, and a walk with no room for a record are
/// refused: no mark is set, and the cursor is as it was.
#[test]
fn a_mark_or_a_walk_that_cannot_be_done_changes_nothing() -> Result<()> {
    let record = record_bytes(&ONE_INT4)?;
    let chunk_bytes = CHUNK_HEADER + 40 * record;
    let values: Vec<i32> = (0..50).collect();
    let keys = [Column::new(&values, None)];
    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
    let mut table = OwnedTable::new(&ONE_INT4, 50, chunk_bytes)?;
    let refs = insert_all(&mut table, &hashes, &Keys(&keys), None)?;
    let chunks = table.chunk_set()?;
    assert_eq!(chunks.len(), 2, "chunk 1 holds 10 records");
    let marks = TestMarks(
        (0..2)
            .map(|_| vec![std::cell::Cell::new(0); mark_words(chunk_bytes, record).unwrap()])
            .collect(),
    );
    let unmarked = |marks: &TestMarks| marks.0.iter().flatten().all(|word| word.get() == 0);
    let one = all_rows(1);
    let one = RowMaskView::try_new(1, &one)?;
    // Before the records, between two, past the chunk's length, in no
    // chunk.
    for reference in [
        reference(0, 0),
        refs[0] + 1,
        reference(0, chunk_bytes),
        reference(5, CHUNK_HEADER),
    ] {
        assert_eq!(
            refusal(mark(chunks.lens(), record, &[reference], &one, &marks)),
            Some((Kind::WrongCall, Rule::Reference)),
            "{reference:#x}"
        );
    }
    for size in [0, 12, MAX_CHUNK_LEN] {
        assert_eq!(
            refusal(mark(chunks.lens(), size, &refs[..1], &one, &marks)),
            Some((Kind::WrongCall, Rule::RecordBytes)),
            "a mark of {size} bytes"
        );
    }
    assert!(unmarked(&marks), "a refused mark set one");
    let at = |chunk, byte| Cursor::at(chunk, byte);
    for (size, cursor, room, rule) in [
        (0, at(0, CHUNK_HEADER), 4, Rule::RecordBytes),
        (12, at(0, CHUNK_HEADER), 4, Rule::RecordBytes),
        (MAX_CHUNK_LEN, at(0, CHUNK_HEADER), 4, Rule::RecordBytes),
        (record, at(0, CHUNK_HEADER + 12), 4, Rule::Cursor),
        (
            record,
            at(1, CHUNK_HEADER + 11 * record),
            4,
            Rule::CursorPast,
        ),
        (record, at(0, CHUNK_HEADER), 0, Rule::EmptyWalk),
    ] {
        let mut moved = cursor;
        let mut out = vec![0; room];
        assert_eq!(
            refusal(scan_unmarked(
                &chunks,
                size,
                &mut moved,
                Some(&marks),
                &mut out
            )),
            Some((Kind::WrongCall, rule)),
            "{rule:?}"
        );
        assert!(moved == cursor, "{rule:?}: the cursor moved");
    }
    Ok(())
}
