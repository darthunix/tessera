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
    let words = encoder.words();
    let mut access = Access::for_chunks(region, layout);
    let mut count = 0;
    for chunk in 0..region.chunks() {
        let (used, _) = access.room(chunk)?;
        let mut byte = CHUNK_HEADER;
        while byte < used {
            let reference = access.reference((chunk, byte));
            let view = access.locate(reference)?;
            let at = count * words;
            ensure!(
                at + words <= items.len(),
                "{} words do not hold the items of the table's records",
                items.len()
            );
            encoder.encode(
                view.keys(),
                view.null_bits(),
                reference,
                &mut items[at..at + words],
            )?;
            count += 1;
            byte += layout.record_size;
        }
    }
    Ok(count)
}
