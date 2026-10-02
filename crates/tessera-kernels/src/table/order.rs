//! The sort items of a table's records: every record of every chunk, in
//! the order appended, as an item of [`crate::sort`] made of its key slots,
//! its NULL bits and its reference. The records need not be linked.

use anyhow::{Result, bail, ensure};

use super::header::{CHUNK_HEADER, Layout};
use super::record::Access;
use super::region::Region;
use tessera_core::RowMaskView;

use crate::sort::{Encoder, SortKey, heap_push};

/// Write the item of every record into `items`, one after another, and
/// return the count; `items` must hold them all.
pub(super) fn items<R: Region>(
    region: &R,
    layout: &Layout,
    keys: &[SortKey],
    items: &mut [u64],
) -> Result<usize> {
    ensure!(
        keys.len() == layout.nkeys
            && keys
                .iter()
                .zip(&layout.kinds[..layout.nkeys])
                .all(|(key, &kind)| key.kind == kind),
        "the sort keys are not the table's keys"
    );
    let encoder = Encoder::new(keys)?;
    macro_rules! dispatch {
        ($($n:literal)*) => {
            match encoder.words() {
                $($n => items_as::<R, $n>(region, layout, &encoder, items),)*
                words => bail!("an item has at most 17 words, not {words}"),
            }
        };
    }
    dispatch!(1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17)
}

/// [`items`] for items of `W` words: the records of each chunk are read
/// one after another up to its used mark, which [`Access::room`] checked
/// to be a record boundary within the chunk.
fn items_as<R: Region, const W: usize>(
    region: &R,
    layout: &Layout,
    encoder: &Encoder<'_>,
    items: &mut [u64],
) -> Result<usize> {
    let (items, _) = items.as_chunks_mut::<W>();
    let access = Access::for_chunks(region, layout);
    let record_size = layout.record_size;
    let mut count = 0;
    for chunk in 0..region.chunks() {
        let (used, _) = access.room(chunk)?;
        ensure!(
            items.len() - count >= (used - CHUNK_HEADER) / record_size,
            "{} items do not hold the table's records",
            items.len()
        );
        let mut byte = CHUNK_HEADER;
        while byte < used {
            // SAFETY: the place lies below the chunk's used mark, a record
            // boundary `room` checked against the chunk's length, and the
            // chunk's one writer wrote whole records up to it.
            let view = unsafe { access.view_at(access.spot((chunk, byte))) };
            ensure!(
                view.len() == record_size,
                "table chunk {chunk} holds no record at byte {byte}"
            );
            items[count] = encoder.encode::<W>(
                view.keys(),
                view.null_bits(),
                access.reference((chunk, byte)),
            )?;
            count += 1;
            byte += record_size;
        }
    }
    Ok(count)
}

/// Push the item of each record `refs[row]` of `rows` into a top-N heap:
/// `heap` holds its capacity of items one after another, the first `*len`
/// of them the heap; a record better than the top replaces it once the heap
/// is full (see [`crate::sort::heap_push`]).
pub(super) fn top_push<R: Region>(
    region: &R,
    layout: &Layout,
    keys: &[SortKey],
    refs: &[u32],
    rows: &RowMaskView<'_>,
    heap: &mut [u64],
    len: &mut usize,
) -> Result<()> {
    ensure!(
        keys.len() == layout.nkeys
            && keys
                .iter()
                .zip(&layout.kinds[..layout.nkeys])
                .all(|(key, &kind)| key.kind == kind),
        "the sort keys are not the table's keys"
    );
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
    macro_rules! dispatch {
        ($($n:literal)*) => {
            match words {
                $($n => push_as::<R, $n>(region, layout, &encoder, refs, rows, heap, len),)*
                words => bail!("an item has at most 17 words, not {words}"),
            }
        };
    }
    dispatch!(1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17)
}

fn push_as<R: Region, const W: usize>(
    region: &R,
    layout: &Layout,
    encoder: &Encoder<'_>,
    refs: &[u32],
    rows: &RowMaskView<'_>,
    heap: &mut [u64],
    len: &mut usize,
) -> Result<()> {
    let (heap, _) = heap.as_chunks_mut::<W>();
    let mut access = Access::new(region, layout);
    for index in 0..rows.nrows().div_ceil(64) {
        let mut bits = rows.word_at(index);
        while bits != 0 {
            let row = index * 64 + bits.trailing_zeros() as usize;
            bits &= bits - 1;
            let reference = refs[row];
            let view = access.locate(reference)?;
            let item = encoder.encode::<W>(view.keys(), view.null_bits(), reference)?;
            heap_push(heap, len, item);
        }
    }
    Ok(())
}
