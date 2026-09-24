//! The batch operations: insertion, probing and the next match of a row.

use core::mem::MaybeUninit;

use anyhow::{Result, ensure};
use tessera_core::{RowMask, RowMaskView};

use super::header::Layout;
use super::keys::{KeySource, WordKeys, slot_buffer};
use super::record::Access;
use super::region::Region;

/// Call `$f` specialized for the common shapes of a table: one or two
/// keys, one or two words after them; 0 stands for any other count, read
/// from the layout at run time. The last parameter is the length of the
/// key slot buffer: the key count, or the maximum for any count.
macro_rules! shaped {
    ($nkeys:expr, $tail:expr, $f:ident($($arg:expr),* $(,)?)) => {
        match ($nkeys, $tail) {
            (1, 1) => $f::<_, _, 1, 1, 1>($($arg),*),
            (1, 2) => $f::<_, _, 1, 2, 1>($($arg),*),
            (1, _) => $f::<_, _, 1, 0, 1>($($arg),*),
            (2, 1) => $f::<_, _, 2, 1, 2>($($arg),*),
            (2, 2) => $f::<_, _, 2, 2, 2>($($arg),*),
            (2, _) => $f::<_, _, 2, 0, 2>($($arg),*),
            _ => $f::<_, _, 0, 0, { $crate::table::MAX_KEYS }>($($arg),*),
        }
    };
}
pub(super) use shaped;

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
    shaped!(
        layout.nkeys,
        layout.tail_words(),
        insert_rows(region, layout, hashes, keys, payload, pending, offsets)
    )
}

/// The rows of [`insert`] for a table of `N` keys and `T` words after
/// them, 0 for either when it is not one of the specialized shapes.
#[inline(never)]
fn insert_rows<R: Region, K: KeySource + ?Sized, const N: usize, const T: usize, const L: usize>(
    region: &R,
    layout: &Layout,
    hashes: &[u32],
    keys: &K,
    payload: Option<&[u8]>,
    pending: &mut RowMask<'_>,
    offsets: &mut [u32],
) -> Result<usize> {
    // Made here, not passed in, so that its fields stay in registers.
    let mut access = Access::new(region, layout);
    let nrows = pending.as_view().nrows();
    let payload_size = access.payload_size();
    let mut buffer = slot_buffer::<L>();
    let mut word_keys = WordKeys::new(&mut buffer, access.nkeys());
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
            let hash = hashes[row];
            // SAFETY: `byte` starts the `slot`-th of the `count` records
            // just reserved; the buffer and the shape are this table's.
            unsafe { access.write::<N, T>(byte, hash, &word_keys, bit, row_payload) };
            access.push(offset, byte, hash);
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
    shaped!(
        layout.nkeys,
        0,
        probe_rows(region, layout, hashes, keys, rows, matches, found)
    )
}

/// The rows of [`probe`] for a table of `N` keys, 0 when it is not one of
/// the specialized counts; `T` is unused.
#[inline(never)]
fn probe_rows<R: Region, K: KeySource + ?Sized, const N: usize, const T: usize, const L: usize>(
    region: &R,
    layout: &Layout,
    hashes: &[u32],
    keys: &K,
    rows: &RowMaskView<'_>,
    matches: &mut [u32],
    found: &mut RowMask<'_>,
) -> Result<()> {
    // Made here, not passed in, so that its fields stay in registers.
    let mut access = Access::new(region, layout);
    let nrows = rows.nrows();
    let mut buffer = slot_buffer::<L>();
    let mut word_keys = WordKeys::new(&mut buffer, access.nkeys());
    let mut lanes = Lanes::new();
    for index in 0..nrows.div_ceil(64) {
        let selected = rows.word(index).unwrap();
        let mut hits = 0;
        if selected.count_ones() >= VERTICAL_MIN_ROWS {
            word_keys.load(keys, index, selected)?;
            let base = index * 64;
            let end = nrows.min(base + 64);
            hits = probe_word::<R, N>(
                &mut access,
                &word_keys,
                &hashes[base..end],
                selected,
                &mut matches[base..end],
                &mut lanes,
            )?;
        } else if selected != 0 {
            word_keys.load(keys, index, selected)?;
            let mut bits = selected;
            while bits != 0 {
                let bit = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                let row = index * 64 + bit;
                let hash = hashes[row];
                let head = access.head(hash);
                // SAFETY: the records are this table's, whose key count
                // the buffer was made for and `N` is 0 or.
                let offset = access.find(head, hash, |record| unsafe {
                    word_keys.equal::<N>(bit, record)
                })?;
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

/// Rows of a word from which a probe goes vertically, phase by phase over
/// the word, rather than row by row: fewer rows do not pay for phases
/// that run over all 64.
pub(super) const VERTICAL_MIN_ROWS: u32 = 8;

/// The per-row arrays of a word's vertical probe, indexed by row within
/// the word: the rows' hashes and the records each row is at. A probe
/// writes a row's entries before it reads them, for the selected rows
/// only, so the arrays start uninitialized and are never cleared.
pub(super) struct Lanes {
    hash: [MaybeUninit<u32>; 64],
    current: [MaybeUninit<u32>; 64],
}

impl Lanes {
    #[inline(always)]
    pub(super) fn new() -> Self {
        Self {
            hash: [MaybeUninit::uninit(); 64],
            current: [MaybeUninit::uninit(); 64],
        }
    }
}

/// The rows of `bits`, lowest first.
#[inline(always)]
fn rows_of(mut bits: u64) -> impl Iterator<Item = usize> {
    core::iter::from_fn(move || {
        (bits != 0).then(|| {
            let bit = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            bit
        })
    })
}

/// Probe the `selected` rows of one word phase by phase: every row's bucket
/// is hinted to the cache, then every head read; while rows remain, every
/// candidate record is checked and hinted, then every candidate compared
/// with its row and the rows that did not match moved down their chains.
/// The loads of different rows are independent and in flight together,
/// which a row-by-row walk, with its longer path per row, allows for fewer
/// rows at a time. A candidate is compared field by field as it is read:
/// gathering the fields into arrays for a vector comparison made vector
/// loads wait on the scalar stores that had just filled them. The answer is the
/// row-by-row one: each row gets the first record of its chain with its
/// hash, null bits and keys. Returns the rows found.
#[inline(always)]
pub(super) fn probe_word<R: Region, const N: usize>(
    access: &mut Access<'_, R>,
    word_keys: &WordKeys<'_>,
    hashes: &[u32],
    selected: u64,
    matches: &mut [u32],
    lanes: &mut Lanes,
) -> Result<u64> {
    for bit in rows_of(selected) {
        let hash = hashes[bit];
        lanes.hash[bit].write(hash);
        access.prefetch_bucket(hash);
    }
    // SAFETY (every `assume_init` below): the entries of a row are read
    // only for rows of `selected` (`pending` is a subset of it), after the
    // loop above wrote its hash and the loop below its first offset.
    let mut pending = 0;
    for bit in rows_of(selected) {
        let head = access.head(unsafe { lanes.hash[bit].assume_init() });
        lanes.current[bit].write(head);
        pending |= u64::from(head != 0) << bit;
    }
    let mut hits = 0;
    let mut steps = 0;
    while pending != 0 {
        access.check_steps(steps)?;
        steps += 1;
        for bit in rows_of(pending) {
            let byte = access.place(unsafe { lanes.current[bit].assume_init() })?;
            access.prefetch_record(byte);
        }
        let mut rest = 0;
        for bit in rows_of(pending) {
            let offset = unsafe { lanes.current[bit].assume_init() };
            // SAFETY: `place` accepted every offset of `pending` above.
            let record = unsafe { access.open(offset) }?;
            // SAFETY: the record is this table's, whose key count the
            // buffer was made for and `N` is 0 or.
            let hash = unsafe { lanes.hash[bit].assume_init() };
            if record.hash() == hash && unsafe { word_keys.equal::<N>(bit, &record) } {
                matches[bit] = offset;
                hits |= 1 << bit;
            } else {
                let next = record.next();
                lanes.current[bit].write(next);
                rest |= u64::from(next != 0) << bit;
            }
        }
        pending = rest;
    }
    Ok(hits)
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
