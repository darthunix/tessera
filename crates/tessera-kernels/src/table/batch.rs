//! The batch operations: insertion, with probing to follow.

use anyhow::{Result, ensure};
use tessera_core::RowMask;

use super::header::Layout;
use super::keys::{KeySource, WordKeys};
use super::record::Access;
use super::region::Region;

/// Reject a batch whose keys or buffers do not match the table and the
/// mask, before anything is read or changed.
fn check<K: KeySource + ?Sized>(
    layout: &Layout,
    keys: &K,
    nrows: usize,
    hashes: usize,
    out: usize,
) -> Result<()> {
    ensure!(
        keys.nkeys() == layout.nkeys,
        "the table has {} keys, the batch {}",
        layout.nkeys,
        keys.nkeys()
    );
    ensure!(
        keys.nrows() == nrows && hashes == nrows && out == nrows,
        "the keys, hashes, mask and offsets of the batch have different row counts"
    );
    Ok(())
}

/// Insert the rows of `pending`, in row order, until the table has no
/// room: inserted rows leave `pending` and get their record offsets in
/// `offsets`. The count inserted is returned.
pub(super) fn insert<R: Region, K: KeySource + ?Sized>(
    region: &R,
    layout: &Layout,
    hashes: &[u32],
    keys: &K,
    payload: Option<&[u8]>,
    pending: &mut RowMask<'_>,
    offsets: &mut [u32],
) -> Result<usize> {
    let nrows = pending.as_view().nrows();
    check(layout, keys, nrows, hashes.len(), offsets.len())?;
    let payload_size = layout.payload_size;
    if let Some(payload) = payload {
        ensure!(
            nrows.checked_mul(payload_size) == Some(payload.len()),
            "the payload has {} bytes, not {payload_size} per row of {nrows}",
            payload.len()
        );
    }
    let mut access = Access::new(region, layout);
    let mut word_keys = WordKeys::new();
    let mut inserted = 0;
    for index in 0..nrows.div_ceil(64) {
        let selected = pending.as_view().word(index).unwrap();
        if selected == 0 {
            continue;
        }
        word_keys.load(keys, layout.nkeys, index, selected)?;
        let wanted = selected.count_ones() as usize;
        let Some((start, count)) = access.reserve(wanted) else {
            break;
        };
        let mut bits = selected;
        let mut done = 0;
        for slot in 0..count {
            let bit = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            let row = index * 64 + bit;
            let byte = start + slot * layout.record_size;
            let offset = (byte / 8) as u32;
            access.write(
                byte,
                hashes[row],
                word_keys.null_bits(bit),
                word_keys.keys(bit, layout.nkeys),
                payload.map(|payload| &payload[row * payload_size..(row + 1) * payload_size]),
            );
            access.push(offset, byte, hashes[row]);
            offsets[row] = offset;
            done |= 1 << bit;
        }
        access.count(count);
        pending.intersect_word(index, !done)?;
        inserted += count;
        if count < wanted {
            break;
        }
    }
    Ok(inserted)
}
