//! Calls that place rows and fail part way: the rows placed before the
//! error stay placed, the masks say so, and each chunk's used mark covers
//! their records, so that the batch given again places the rest once.

use anyhow::Result;

use super::*;
use crate::table::testing::{Column, Failing, Keys};

/// The calls that place rows into one chunk.
#[derive(Clone, Copy, Debug)]
enum Call {
    Append,
    Columns,
    Find,
}

/// A table of one int4 key and a payload of one column, its NULL word
/// and its value.
const ONE_COLUMN: TableConfig = TableConfig {
    nkeys: 1,
    payload_size: 16,
};

/// Place the pending rows of keys 0 to 127 into `chunk` by `call`; a find
/// adds the rows that made a record to `inserted`.
fn place(
    table: &mut OwnedTable,
    call: Call,
    chunk: usize,
    keys: &impl KeySource,
    pending: &mut [u64],
    refs: &mut [u32],
    inserted: &mut [u64],
) -> crate::error::Result<usize> {
    let hashes: Vec<u32> = (0..128).map(hash_i32).collect();
    let values: Vec<u64> = (0..128).collect();
    let nulls = [false; 128];
    let value_columns: [&[u64]; 1] = [&values];
    let null_columns: [&[bool]; 1] = [&nulls];
    let columns = PayloadColumns::new(&value_columns, &null_columns, 128)?;
    let mut batch = Batch::new(&hashes, keys, pending, refs)?;
    match call {
        Call::Append => table.table_mut()?.append(chunk, None, &mut batch),
        Call::Columns => table
            .table_mut()?
            .append_columns(chunk, &columns, &mut batch),
        Call::Find => table.table_mut()?.find_or_insert(
            chunk,
            &mut batch,
            &mut RowMask::try_new(128, inserted)?,
        ),
    }
}

#[test]
fn a_source_that_fails_at_the_second_word_leaves_the_first_placed() -> Result<()> {
    let values: Vec<i32> = (0..128).collect();
    let keys = [Column::new(&values, None)];
    for call in [Call::Append, Call::Columns, Call::Find] {
        let mut table = OwnedTable::new(&ONE_COLUMN, 256, CHUNK_HEADER + 128 * 40)?;
        let chunk = table.add_chunk()?;
        let mut pending = [u64::MAX; 2];
        let mut refs = [0; 128];
        let mut inserted = [0; 2];
        let failing = Failing(&Keys(&keys), 1);
        let failed = place(
            &mut table,
            call,
            chunk,
            &failing,
            &mut pending,
            &mut refs,
            &mut inserted,
        );
        assert_eq!(
            refusal(failed),
            Some((Kind::WrongCall, Rule::KeyRead)),
            "{call:?}"
        );
        assert_eq!(pending, [0, u64::MAX], "{call:?}: the first word left");
        assert_eq!(
            scan_all(&mut table, 64)?,
            refs[..64],
            "{call:?}: the chunk's mark covers the first word"
        );
        let shared = table.table()?;
        for (row, &reference) in refs[..64].iter().enumerate() {
            let record = shared.record(reference)?;
            assert_eq!(record.keys, [row as i64], "{call:?}: row {row}");
        }
        if let Call::Find = call {
            assert_eq!(inserted, [u64::MAX, 0]);
            assert_eq!(table.table()?.stats().records, 64);
        }
        // Given again with every word, the batch places the second word
        // after the first.
        place(
            &mut table,
            call,
            chunk,
            &Keys(&keys),
            &mut pending,
            &mut refs,
            &mut inserted,
        )?;
        assert_eq!(pending, [0, 0], "{call:?}");
        assert_eq!(scan_all(&mut table, 64)?, refs, "{call:?}: each row once");
        if let Call::Find = call {
            assert_eq!(inserted, [u64::MAX; 2]);
            assert_eq!(table.table()?.stats().records, 128);
        }
    }
    Ok(())
}

/// Place the pending rows of keys 0 to 19 by partition: a find, or an
/// append of their payload columns.
#[allow(clippy::too_many_arguments)]
fn place_by_partition(
    table: &mut OwnedTable,
    find: bool,
    partitions: &Partitions<'_>,
    hashes: &[u32],
    pending: &mut [u64],
    refs: &mut [u32],
    inserted: &mut [u64],
    rows: &mut [u64],
) -> crate::error::Result<usize> {
    let values: Vec<i32> = (0..20).collect();
    let keys = [Column::new(&values, None)];
    let source = Keys(&keys);
    let payload: Vec<u64> = (0..20).collect();
    let nulls = [false; 20];
    let value_columns: [&[u64]; 1] = [&payload];
    let null_columns: [&[bool]; 1] = [&nulls];
    let columns = PayloadColumns::new(&value_columns, &null_columns, 20)?;
    let mut batch = Batch::new(hashes, &source, pending, refs)?;
    if find {
        table.table_mut()?.find_or_insert_partitioned(
            partitions,
            &mut batch,
            &mut RowMask::try_new(20, inserted)?,
        )
    } else {
        let mut nulls = 0;
        super::writer(&ONE_COLUMN, table.chunk_set()?)?.append_partitioned_columns(
            partitions,
            &columns,
            &mut batch,
            &mut Appended {
                rows,
                nulls: &mut nulls,
            },
        )
    }
}

#[test]
fn a_partition_with_a_damaged_mark_leaves_the_rows_before_placed() -> Result<()> {
    // Rows 0 to 9 go to partition 0 by their hashes, rows 10 to 19 to
    // partition 1, whose chunk's used mark ends no record.
    let hashes: Vec<u32> = (0..20).map(|row| u32::from(row >= 10)).collect();
    for find in [false, true] {
        let mut table = OwnedTable::new(&ONE_COLUMN, 64, CHUNK_HEADER + 32 * 40)?;
        let chunks = [table.add_chunk()? as u32, table.add_chunk()? as u32];
        table.chunk_words(chunks[1] as usize)[0] = 12;
        let partitions = Partitions {
            shift: 0,
            chunks: &chunks,
        };
        let mut pending = [(1 << 20) - 1];
        let mut refs = [0; 20];
        let mut inserted = [0];
        let mut rows = [0; 2];
        let failed = place_by_partition(
            &mut table,
            find,
            &partitions,
            &hashes,
            &mut pending,
            &mut refs,
            &mut inserted,
            &mut rows,
        );
        assert_eq!(
            refusal(failed),
            Some((Kind::Damaged, Rule::UsedMark)),
            "find {find}"
        );
        assert_eq!(pending, [((1 << 20) - 1) ^ ((1 << 10) - 1)], "find {find}");
        if find {
            assert_eq!(inserted, [(1 << 10) - 1]);
            assert_eq!(table.table()?.stats().records, 10);
        } else {
            assert_eq!(rows, [10, 0]);
        }
        // The mark repaired, the batch given again places the rest once.
        table.chunk_words(chunks[1] as usize)[0] = CHUNK_HEADER as u64;
        let placed = place_by_partition(
            &mut table,
            find,
            &partitions,
            &hashes,
            &mut pending,
            &mut refs,
            &mut inserted,
            &mut rows,
        )?;
        assert_eq!((placed, pending), (10, [0]), "find {find}");
        let mut walked = scan_all(&mut table, 64)?;
        walked.sort_unstable();
        let mut placed = refs.to_vec();
        placed.sort_unstable();
        assert_eq!(walked, placed, "find {find}: each row once");
        if find {
            assert_eq!(inserted, [(1 << 20) - 1]);
            assert_eq!(table.table()?.stats().records, 20);
        } else {
            assert_eq!(rows, [10, 10]);
        }
    }
    Ok(())
}

#[test]
fn a_link_that_meets_a_damaged_record_counts_what_it_linked() -> Result<()> {
    let values: Vec<i32> = (0..4).collect();
    let keys = [Column::new(&values, None)];
    let hashes: Vec<u32> = values.iter().map(|&value| hash_i32(value)).collect();
    let mut table = local(&ONE_INT4, 4)?;
    let chunk = table.add_chunk()?;
    let mut pending = [0b1111];
    let mut refs = [0; 4];
    table.table_mut()?.append(
        chunk,
        None,
        &mut Batch::new(&hashes, &Keys(&keys), &mut pending, &mut refs)?,
    )?;
    // Record 2 claims another size, in words of 8 bytes.
    let words = table.table()?.record_size() as u32 / 8;
    let (_, byte) = placement(refs[2]);
    set_u32(table.chunk_words(chunk), byte + 12, words + 1);
    let mut from = CHUNK_HEADER;
    let failed = table.table_mut()?.link(chunk, &mut from);
    assert_eq!(refusal(failed), Some((Kind::Damaged, Rule::NoRecord)));
    assert_eq!(table.table()?.stats().records, 2, "the records linked");
    assert_eq!(from, byte, "the next link starts at the damaged record");
    set_u32(table.chunk_words(chunk), byte + 12, words);
    assert_eq!(table.table_mut()?.link(chunk, &mut from)?, 2);
    assert_eq!(table.table()?.stats().records, 4);
    let (found, matches) = probe_all(&table.table()?, &hashes, &Keys(&keys))?;
    assert_eq!(found, [0, 1, 2, 3]);
    assert_eq!(matches, refs);
    Ok(())
}
