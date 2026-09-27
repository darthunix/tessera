//! The sort items of a table's records: every record of every chunk, in
//! the order appended, as an item of [`crate::sort`] made of its key slots,
//! its NULL bits and its reference. The records need not be linked.

use anyhow::{Result, ensure};

use super::header::{CHUNK_HEADER, Layout};
use super::record::Access;
use super::region::Region;
use crate::sort::{Encoder, SortKey};

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
                words => unreachable!("an item has at most 17 words, not {words}"),
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
