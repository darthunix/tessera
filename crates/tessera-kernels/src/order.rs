//! The sort items of a table's records: every record of every chunk, in
//! the order appended, as an item of [`crate::sort`] made of its key slots,
//! its NULL bits and its reference. The records need not be linked. The
//! items take what the table offers its callers alone: references from
//! its scan, and keys from its cursor that reads records.
//!
//! Each call is a public function over a private body, as the table's
//! methods were over these bodies: LLVM then optimizes the body alone and
//! again where it is inlined, and the items' loops compile as they did
//! inside the table; a body made public itself compiles otherwise.

use anyhow::{Result, ensure};

use crate::table::{Cursor, Table, TableMut};
use tessera_core::{RowMaskView, ones};

use crate::sort::{Encoder, SortKey, heap_push, with_item_words};

/// The references a scan gives at a time to the items.
const SCAN: usize = 64;

/// Refuse sort keys that are not the table's keys, one each.
fn check_keys(table: &Table<'_>, keys: &[SortKey]) -> Result<()> {
    ensure!(
        keys.len() == table.config().nkeys,
        "the sort keys are not the table's keys"
    );
    Ok(())
}

/// Write the item of every record into `items`, one after another, and
/// return the count; `items` must hold them all. The walk over the records
/// needs the table to itself.
pub fn items(table: &TableMut<'_>, keys: &[SortKey], items: &mut [u64]) -> Result<usize> {
    items_inner(table, keys, items)
}

fn items_inner(table: &TableMut<'_>, keys: &[SortKey], items: &mut [u64]) -> Result<usize> {
    check_keys(table, keys)?;
    let encoder = Encoder::new(keys)?;
    with_item_words!(encoder.words(), |W| items_as::<W>(table, &encoder, items))
}

/// [`items`] for items of `W` words: the scan gives the records'
/// references a few at a time, and the reading cursor their keys.
fn items_as<const W: usize>(
    table: &TableMut<'_>,
    encoder: &Encoder<'_>,
    items: &mut [u64],
) -> Result<usize> {
    let (items, _) = items.as_chunks_mut::<W>();
    let mut records = table.records();
    let mut cursor = Cursor::start();
    let mut refs = [0_u32; SCAN];
    let mut count = 0;
    loop {
        let scanned = table.scan(&mut cursor, &mut refs)?;
        if scanned == 0 {
            return Ok(count);
        }
        ensure!(
            items.len() - count >= scanned,
            "{} items do not hold the table's records",
            items.len()
        );
        for &reference in &refs[..scanned] {
            let record = records.record(reference)?;
            items[count] = encoder.encode::<W>(record.keys, record.null_bits, reference)?;
            count += 1;
        }
    }
}

/// Push the item of each record `refs[row]` of `rows` into a top-N heap:
/// `heap` holds its capacity of items one after another, the first `*len`
/// of them the heap; a record better than the top replaces it once the heap
/// is full (see `heap_push` of [`crate::sort`]).
pub fn top_push(
    table: &Table<'_>,
    keys: &[SortKey],
    refs: &[u32],
    rows: &RowMaskView<'_>,
    heap: &mut [u64],
    len: &mut usize,
) -> Result<()> {
    top_push_inner(table, keys, refs, rows, heap, len)
}

fn top_push_inner(
    table: &Table<'_>,
    keys: &[SortKey],
    refs: &[u32],
    rows: &RowMaskView<'_>,
    heap: &mut [u64],
    len: &mut usize,
) -> Result<()> {
    check_keys(table, keys)?;
    ensure!(
        refs.len() == rows.nrows(),
        "the references and the mask have different row counts"
    );
    let encoder = Encoder::new(keys)?;
    let words = encoder.words();
    ensure!(
        heap.len().is_multiple_of(words) && *len <= heap.len() / words,
        "a heap of {} words and {} items does not hold items of {words} words",
        heap.len(),
        *len
    );
    with_item_words!(words, |W| push_as::<W>(
        table, &encoder, refs, rows, heap, len
    ))
}

fn push_as<const W: usize>(
    table: &Table<'_>,
    encoder: &Encoder<'_>,
    refs: &[u32],
    rows: &RowMaskView<'_>,
    heap: &mut [u64],
    len: &mut usize,
) -> Result<()> {
    let (heap, _) = heap.as_chunks_mut::<W>();
    let mut records = table.records();
    for index in 0..rows.nrows().div_ceil(64) {
        let bits = rows.word(index);
        for bit in ones(bits) {
            let row = index * 64 + bit;
            let reference = refs[row];
            let record = records.record(reference)?;
            let item = encoder.encode::<W>(record.keys, record.null_bits, reference)?;
            heap_push(heap, len, item);
        }
    }
    Ok(())
}
