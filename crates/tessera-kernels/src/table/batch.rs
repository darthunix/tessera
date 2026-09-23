//! The batch operations: insertion, probing and the next match of a row.

use anyhow::{Result, ensure};
use tessera_core::{RowMask, RowMaskView};

use super::header::Layout;
use super::keys::{KeySource, WordKeys};
use super::record::Access;
use super::region::Region;

/// Reject a batch whose keys or buffers do not match the table and the
/// mask, before anything is read or changed.
pub(super) fn check<K: KeySource + ?Sized>(
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
    let mut word_keys = WordKeys::new(layout.nkeys);
    let mut inserted = 0;
    for index in 0..nrows.div_ceil(64) {
        let selected = pending.as_view().word(index).unwrap();
        if selected == 0 {
            continue;
        }
        word_keys.load(keys, index, selected)?;
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
            let byte = start + slot * access.record_size();
            let offset = (byte / 8) as u32;
            // SAFETY: the payload was checked to hold `payload_size` bytes
            // for each of the `nrows` rows, and `row` is below `nrows`.
            let row_payload = payload.map(|payload| unsafe {
                payload.get_unchecked(row * payload_size..(row + 1) * payload_size)
            });
            access.write(byte, hashes[row], &word_keys, bit, row_payload);
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

/// Find the first record with the hash and keys of each row of `rows`:
/// `matches[row]` gets its offset and `found` the rows that have one.
pub(super) fn probe<R: Region, K: KeySource + ?Sized>(
    region: &R,
    layout: &Layout,
    hashes: &[u32],
    keys: &K,
    rows: &RowMaskView<'_>,
    matches: &mut [u32],
    found: &mut RowMask<'_>,
) -> Result<()> {
    let nrows = rows.nrows();
    check(layout, keys, nrows, hashes.len(), matches.len())?;
    ensure!(
        found.as_view().nrows() == nrows,
        "the result mask has {} rows, the batch {nrows}",
        found.as_view().nrows()
    );
    let mut access = Access::new(region, layout);
    let mut word_keys = WordKeys::new(layout.nkeys);
    for index in 0..nrows.div_ceil(64) {
        let selected = rows.word(index).unwrap();
        let mut hits = 0;
        if selected != 0 {
            word_keys.load(keys, index, selected)?;
            let mut bits = selected;
            while bits != 0 {
                let bit = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                let row = index * 64 + bit;
                let hash = hashes[row];
                let head = access.head(hash);
                let offset = access.find(head, hash, |record| word_keys.equal(bit, record))?;
                if offset != 0 {
                    matches[row] = offset;
                    hits |= 1 << bit;
                }
            }
        }
        found.set_word(index, hits)?;
    }
    Ok(())
}

/// For each row of `rows`, replace `offsets[row]` by the record after it
/// in its chain with the same hash and keys; `found` gets the rows that
/// have one, and the others keep their offset.
pub(super) fn next_match<R: Region>(
    region: &R,
    layout: &Layout,
    offsets: &mut [u32],
    rows: &RowMaskView<'_>,
    found: &mut RowMask<'_>,
) -> Result<()> {
    let nrows = rows.nrows();
    ensure!(
        offsets.len() == nrows && found.as_view().nrows() == nrows,
        "the offsets, mask and result of the batch have different row counts"
    );
    let mut access = Access::new(region, layout);
    for index in 0..nrows.div_ceil(64) {
        let mut bits = rows.word(index).unwrap();
        let mut hits = 0;
        while bits != 0 {
            let bit = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            let row = index * 64 + bit;
            let record = access.locate(offsets[row])?;
            let (hash, null_bits, keys) = (record.hash(), record.null_bits(), record.keys());
            let after = access.find(record.next(), hash, |other| {
                other.null_bits() == null_bits && other.keys() == keys
            })?;
            if after != 0 {
                offsets[row] = after;
                hits |= 1 << bit;
            }
        }
        found.set_word(index, hits)?;
    }
    Ok(())
}
