//! What one writer alone may do: find or create the record of a key,
//! insert records next to those of the same key, change payloads, walk
//! the records and grow the region.
//!
//! These need the region to itself: a record found may be updated in
//! place, a walk reads every record below the used mark, which an
//! insertion in flight would have reserved but not written, and growth
//! rewrites the header and the buckets.

use anyhow::{Result, ensure};
use tessera_core::{ColumnReader, RowMask, RowMaskView};

use crate::ops::ArithmeticError;

use super::batch::{Lanes, VERTICAL_MIN_ROWS, check, probe_word, shaped};
use super::header::{CHUNK_USED, HEADER_SIZE, Header, KEY_SLOT, Layout, RECORD_HEADER};
use super::keys::{KeySource, WordKeys, slot_buffer};
use super::record::{Access, same_keys};
use super::region::Region;

/// Where a walk over the records stands: the byte offset of the next
/// record to visit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Cursor(u64);

impl Cursor {
    /// Before the first record.
    pub fn start() -> Self {
        Self(HEADER_SIZE as u64)
    }

    /// The cursor a caller stored as an integer.
    pub fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// The cursor as an integer to store.
    pub fn raw(self) -> u64 {
        self.0
    }
}

/// Give each row of `pending` the record of its keys, creating one with a
/// zero payload where none exists, until the table has no room for a new
/// one: resolved rows leave `pending`, get their offsets in `offsets`, and
/// the rows whose record this call created form `inserted`.
pub(super) fn find_or_insert<R: Region, K: KeySource + ?Sized>(
    region: &R,
    layout: &Layout,
    hashes: &[u32],
    keys: &K,
    pending: &mut RowMask<'_>,
    offsets: &mut [u32],
    inserted: &mut RowMask<'_>,
) -> Result<usize> {
    let nrows = pending.as_view().nrows();
    check(layout, keys, nrows, hashes.len(), offsets.len())?;
    ensure!(
        inserted.as_view().nrows() == nrows,
        "the inserted mask has {} rows, the batch {nrows}",
        inserted.as_view().nrows()
    );
    shaped!(
        layout.nkeys,
        layout.tail_words(),
        resolve_rows(region, layout, hashes, keys, pending, offsets, inserted)
    )
}

/// The rows of [`find_or_insert`] for a table of `N` keys and `T` words
/// after them, 0 for either when it is not one of the specialized shapes.
#[inline(never)]
fn resolve_rows<
    R: Region,
    K: KeySource + ?Sized,
    const N: usize,
    const T: usize,
    const L: usize,
>(
    region: &R,
    layout: &Layout,
    hashes: &[u32],
    keys: &K,
    pending: &mut RowMask<'_>,
    offsets: &mut [u32],
    inserted: &mut RowMask<'_>,
) -> Result<usize> {
    // Made here, not passed in, so that its fields stay in registers.
    let mut access = Access::new(region, layout);
    let nrows = pending.as_view().nrows();
    let mut buffer = slot_buffer::<L>();
    let mut word_keys = WordKeys::new(&mut buffer, access.nkeys());
    let mut lanes = Lanes::new();
    let mut resolved = 0;
    let mut full = false;
    for index in 0..nrows.div_ceil(64) {
        let selected = pending.as_view().word(index).unwrap();
        let mut done = 0;
        let mut created = 0;
        if selected != 0 && !full {
            word_keys.load(keys, index, selected)?;
            // Rows whose record exists are found for the whole word at
            // once; the others then go in row order, so that a row finds
            // the record an earlier row of the word created.
            let mut known = 0;
            if selected.count_ones() >= VERTICAL_MIN_ROWS {
                let base = index * 64;
                let end = nrows.min(base + 64);
                known = probe_word::<R, N>(
                    &mut access,
                    &word_keys,
                    &hashes[base..end],
                    selected,
                    &mut offsets[base..end],
                    &mut lanes,
                )?;
            }
            let mut bits = selected;
            while bits != 0 {
                let bit = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                if known >> bit & 1 != 0 {
                    done |= 1 << bit;
                    resolved += 1;
                    continue;
                }
                let row = index * 64 + bit;
                let hash = hashes[row];
                let head = access.head(hash);
                // SAFETY: the records are this table's, whose key count the
                // buffer was made for and `N` is 0 or.
                let found = access.find(head, hash, |record| unsafe {
                    word_keys.equal::<N>(bit, record)
                })?;
                offsets[row] = match found {
                    0 => {
                        let Some((byte, _)) = access.reserve(1) else {
                            full = true;
                            break;
                        };
                        let offset = (byte / 8) as u32;
                        // SAFETY: `byte` starts the record just reserved;
                        // the buffer and the shape are this table's.
                        unsafe { access.write::<N, T>(byte, hash, &word_keys, bit, None) };
                        access.push(offset, byte, hash);
                        access.count(1);
                        created |= 1 << bit;
                        offset
                    }
                    offset => offset,
                };
                done |= 1 << bit;
                resolved += 1;
            }
        }
        pending.intersect_word(index, !done)?;
        inserted.set_word(index, created)?;
    }
    Ok(resolved)
}

/// Insert the rows of `pending` as new records, in row order, until the
/// table has no room, each right after a record with the same keys when
/// there is one, so that the records of a key lie next to each other in
/// their chain: inserted rows leave `pending` and get their offsets in
/// `offsets`, and the rows whose keys the table already held, from an
/// earlier call or an earlier row, form `duplicates`.
#[allow(clippy::too_many_arguments)]
pub(super) fn insert_grouped<R: Region, K: KeySource + ?Sized>(
    region: &R,
    layout: &Layout,
    hashes: &[u32],
    keys: &K,
    payload: Option<&[u8]>,
    pending: &mut RowMask<'_>,
    offsets: &mut [u32],
    duplicates: &mut RowMask<'_>,
) -> Result<usize> {
    let nrows = pending.as_view().nrows();
    check(layout, keys, nrows, hashes.len(), offsets.len())?;
    ensure!(
        duplicates.as_view().nrows() == nrows,
        "the duplicates mask has {} rows, the batch {nrows}",
        duplicates.as_view().nrows()
    );
    if let Some(payload) = payload {
        ensure!(
            nrows.checked_mul(layout.payload_size) == Some(payload.len()),
            "the payload has {} bytes, not {} per row of {nrows}",
            payload.len(),
            layout.payload_size
        );
    }
    shaped!(
        layout.nkeys,
        layout.tail_words(),
        group_rows(
            region, layout, hashes, keys, payload, pending, offsets, duplicates
        )
    )
}

/// The rows of [`insert_grouped`] for a table of `N` keys and `T` words
/// after them, 0 for either when it is not one of the specialized shapes.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn group_rows<R: Region, K: KeySource + ?Sized, const N: usize, const T: usize, const L: usize>(
    region: &R,
    layout: &Layout,
    hashes: &[u32],
    keys: &K,
    payload: Option<&[u8]>,
    pending: &mut RowMask<'_>,
    offsets: &mut [u32],
    duplicates: &mut RowMask<'_>,
) -> Result<usize> {
    // Made here, not passed in, so that its fields stay in registers.
    let mut access = Access::new(region, layout);
    let nrows = pending.as_view().nrows();
    let payload_size = access.payload_size();
    let mut buffer = slot_buffer::<L>();
    let mut word_keys = WordKeys::new(&mut buffer, access.nkeys());
    let mut lanes = Lanes::new();
    let mut inserted = 0;
    // Filled whole: rows the call does not reach are no duplicates.
    for index in 0..nrows.div_ceil(64) {
        duplicates.set_word(index, 0)?;
    }
    for index in 0..nrows.div_ceil(64) {
        let selected = pending.as_view().word(index).unwrap();
        if selected == 0 {
            continue;
        }
        word_keys.load(keys, index, selected)?;
        let base = index * 64;
        let end = nrows.min(base + 64);
        // The keys the table held before the word are found for the whole
        // word at once; the others are looked up in row order, so that a
        // row finds the record an earlier row of the word created.
        let mut known = 0;
        if selected.count_ones() >= VERTICAL_MIN_ROWS {
            known = probe_word::<R, N>(
                &mut access,
                &word_keys,
                &hashes[base..end],
                selected,
                &mut offsets[base..end],
                &mut lanes,
            )?;
        }
        let wanted = selected.count_ones() as usize;
        let Some((start, count)) = access.reserve(wanted) else {
            break;
        };
        let mut bits = selected;
        let mut done = 0;
        let mut repeated = 0;
        for slot in 0..count {
            let bit = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            let row = base + bit;
            let hash = hashes[row];
            let found = if known >> bit & 1 != 0 {
                offsets[row]
            } else {
                let head = access.head(hash);
                // SAFETY: the records are this table's, whose key count the
                // buffer was made for and `N` is 0 or.
                access.find(head, hash, |record| unsafe {
                    word_keys.equal::<N>(bit, record)
                })?
            };
            let byte = start + slot * access.record_size();
            let offset = (byte / 8) as u32;
            // SAFETY: the payload was checked to hold `payload_size` bytes
            // for each of the `nrows` rows, and `row` is below `nrows`.
            let row_payload = payload.map(|payload| unsafe {
                payload.get_unchecked(row * payload_size..(row + 1) * payload_size)
            });
            // SAFETY: `byte` starts the `slot`-th of the `count` records
            // just reserved; the buffer and the shape are this table's.
            unsafe { access.write::<N, T>(byte, hash, &word_keys, bit, row_payload) };
            if found == 0 {
                access.push(offset, byte, hash);
            } else {
                // SAFETY: the writer has the region to itself; `found` was
                // located by this operation, `byte` written just above.
                unsafe { access.link_after(found, byte, offset) };
                repeated |= 1 << bit;
            }
            // Counted at once: a lookup of a later row may walk this
            // record, and a walk longer than the count is corrupt.
            access.count(1);
            offsets[row] = offset;
            done |= 1 << bit;
        }
        pending.intersect_word(index, !done)?;
        duplicates.set_word(index, repeated)?;
        inserted += count;
        if count < wanted {
            break;
        }
    }
    Ok(inserted)
}

/// The payload of a record, to change in place.
///
/// The exclusive borrow comes from the writer's `&mut TableMut`, which
/// this shared region reference sits behind.
#[allow(clippy::mut_from_ref)]
pub(super) fn payload_mut<'r, R: Region>(
    region: &'r R,
    layout: &'r Layout,
    offset: u32,
) -> Result<&'r mut [u8]> {
    Access::new(region, layout).locate(offset)?;
    let start = offset as usize * 8 + RECORD_HEADER + layout.nkeys * KEY_SLOT;
    // SAFETY: the caller has the region to itself, and the payload lies
    // within a record that `locate` accepted.
    Ok(unsafe { region.bytes_mut(start, layout.payload_size) })
}

/// Visit the records from `cursor` on, in the order they were inserted,
/// as many as `out` holds; the count visited is returned and the cursor
/// moves past them.
pub(super) fn scan<R: Region>(
    region: &R,
    layout: &Layout,
    cursor: &mut Cursor,
    out: &mut [u32],
) -> Result<usize> {
    let used = region.load_u64(CHUNK_USED) as usize;
    let mut byte = usize::try_from(cursor.0).unwrap_or(usize::MAX);
    ensure!(
        byte.is_multiple_of(8) && (HEADER_SIZE..=used).contains(&byte),
        "table cursor {} lies outside the records",
        cursor.0
    );
    let mut access = Access::new(region, layout);
    let mut count = 0;
    while count < out.len() && byte < used {
        let offset = (byte / 8) as u32;
        let len = access.locate(offset)?.len();
        out[count] = offset;
        count += 1;
        byte += len;
    }
    cursor.0 = byte as u64;
    Ok(count)
}

/// Grow the table to the first `new_len` bytes of the region, whose used
/// part must hold what it held: the header takes the new length and a
/// bucket count for it, the buckets are rebuilt at the new end, and the
/// records stay where they are. The new layout is returned.
pub(super) fn grow<R: Region>(region: &R, layout: &Layout, new_len: usize) -> Result<Layout> {
    ensure!(
        new_len <= region.len(),
        "the region has {} bytes, fewer than the {new_len} to grow to",
        region.len()
    );
    let header = Header::load(region).grown(layout.region_len, new_len)?;
    let grown = header.validate(new_len)?;
    header.store(region);
    // SAFETY: the caller has the region to itself, so nothing else reads
    // or writes the buckets while they are cleared.
    unsafe { region.zero_u32(grown.buckets_offset, grown.nbuckets as usize) };
    let used = region.load_u64(CHUNK_USED) as usize;
    let mut access = Access::new(region, &grown);
    let mut byte = HEADER_SIZE;
    // Each record goes after an earlier one with the same keys when there
    // is one, so that the records of a key lie next to each other as
    // grouped insertion left them, and next_in_group still steps through
    // them; a key's first record goes first in its bucket's chain.
    while byte < used {
        let offset = (byte / 8) as u32;
        let record = access.locate(offset)?;
        let (hash, len) = (record.hash(), record.len());
        let (null_bits, keys) = (record.null_bits(), record.keys());
        let head = access.head(hash);
        let same = access.find(head, hash, |other| {
            other.null_bits() == null_bits && same_keys(other.keys(), keys)
        })?;
        if same == 0 {
            access.push(offset, byte, hash);
        } else {
            // SAFETY: the writer has the region to itself; `same` was
            // located by the walk just above, and the record at `byte`
            // is written and holds its keys.
            unsafe { access.link_after(same, byte, offset) };
        }
        byte += len;
    }
    Ok(grown)
}

/// How an aggregate's state in a payload takes a row's value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Fold {
    /// The sum, as an `i64`; an overflow fails with
    /// [`ArithmeticError::BigintOutOfRange`], as PostgreSQL's transition.
    Sum,
    /// The least value.
    Min,
    /// The greatest value.
    Max,
}

/// Where an aggregate keeps its state in a payload: an `i64` at byte
/// `value_at`, and whether it has seen a value as bit `flag_bit` of the
/// `u64` at byte `flags_at`, which several aggregates may share.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Slot {
    pub value_at: usize,
    pub flags_at: usize,
    pub flag_bit: u32,
}

/// Reject offsets, a mask or payload words that do not fit the table.
fn check_accumulate(layout: &Layout, offsets: usize, nrows: usize, words: &[usize]) -> Result<()> {
    ensure!(
        offsets == nrows,
        "the offsets and mask of the batch have different row counts"
    );
    for &at in words {
        ensure!(
            at.is_multiple_of(8)
                && at
                    .checked_add(8)
                    .is_some_and(|end| end <= layout.payload_size),
            "a payload word at byte {at} is past the payload of {} bytes",
            layout.payload_size
        );
    }
    Ok(())
}

/// The payload of the record at `offset`, to change in place.
#[inline(always)]
fn payload_at<'r, R: Region>(
    access: &mut Access<'r, R>,
    region: &'r R,
    layout: &Layout,
    offset: u32,
) -> Result<&'r mut [u8]> {
    access.locate(offset)?;
    let start = offset as usize * 8 + RECORD_HEADER + layout.nkeys * KEY_SLOT;
    // SAFETY: the caller has the region to itself, and the payload lies
    // within a record that `locate` accepted; the slice is dropped before
    // the next row's is made.
    Ok(unsafe { region.bytes_mut(start, layout.payload_size) })
}

#[inline(always)]
fn read_word(payload: &[u8], at: usize) -> u64 {
    let mut word = [0; 8];
    word.copy_from_slice(&payload[at..at + 8]);
    u64::from_ne_bytes(word)
}

#[inline(always)]
fn write_word(payload: &mut [u8], at: usize, value: u64) {
    payload[at..at + 8].copy_from_slice(&value.to_ne_bytes());
}

/// Add one, as an `i64` at byte `at`, to the payload of each selected
/// row's record: `count(*)`.
pub(super) fn count_rows<R: Region>(
    region: &R,
    layout: &Layout,
    offsets: &[u32],
    rows: &RowMaskView<'_>,
    at: usize,
) -> Result<()> {
    let nrows = rows.nrows();
    check_accumulate(layout, offsets.len(), nrows, &[at])?;
    let mut access = Access::new(region, layout);
    for index in 0..nrows.div_ceil(64) {
        let mut bits = rows.word(index).unwrap();
        while bits != 0 {
            let row = index * 64 + bits.trailing_zeros() as usize;
            bits &= bits - 1;
            let payload = payload_at(&mut access, region, layout, offsets[row])?;
            let count = (read_word(payload, at) as i64)
                .checked_add(1)
                .ok_or(ArithmeticError::BigintOutOfRange)?;
            write_word(payload, at, count as u64);
        }
    }
    Ok(())
}

/// Add one, as an `i64` at byte `at`, for each selected row whose value
/// is not NULL: `count(x)`, which reads the NULL flags alone.
pub(super) fn count_values<R: Region, C: ColumnReader + ?Sized>(
    region: &R,
    layout: &Layout,
    offsets: &[u32],
    rows: &RowMaskView<'_>,
    column: &C,
    at: usize,
) -> Result<()> {
    let nrows = rows.nrows();
    check_accumulate(layout, offsets.len(), nrows, &[at])?;
    ensure!(
        column.nrows() == nrows,
        "column and selection row counts differ"
    );
    let mut access = Access::new(region, layout);
    for index in 0..nrows.div_ceil(64) {
        let selected = rows.word(index).unwrap();
        if selected == 0 {
            continue;
        }
        for (row, value) in column.word_values(index, selected)? {
            if value.is_none() {
                continue;
            }
            let payload = payload_at(&mut access, region, layout, offsets[row])?;
            let count = (read_word(payload, at) as i64)
                .checked_add(1)
                .ok_or(ArithmeticError::BigintOutOfRange)?;
            write_word(payload, at, count as u64);
        }
    }
    Ok(())
}

/// Fold each selected row's non-NULL value into the state at `slot` of
/// its record's payload: the first value marks the state as seen, so a
/// group without one stays NULL, as PostgreSQL's strict transitions.
/// Rows go in row order, so an overflow fails where the row-wise sum
/// would.
pub(super) fn fold<R: Region, C, V>(
    region: &R,
    layout: &Layout,
    offsets: &[u32],
    rows: &RowMaskView<'_>,
    column: &C,
    fold: Fold,
    slot: Slot,
) -> Result<()>
where
    C: ColumnReader<Value = V> + ?Sized,
    V: Into<i64> + Copy,
{
    let nrows = rows.nrows();
    check_accumulate(
        layout,
        offsets.len(),
        nrows,
        &[slot.value_at, slot.flags_at],
    )?;
    ensure!(
        slot.flag_bit < 64,
        "flag bit {} is past a word",
        slot.flag_bit
    );
    ensure!(
        column.nrows() == nrows,
        "column and selection row counts differ"
    );
    let flag = 1u64 << slot.flag_bit;
    let mut access = Access::new(region, layout);
    for index in 0..nrows.div_ceil(64) {
        let selected = rows.word(index).unwrap();
        if selected == 0 {
            continue;
        }
        for (row, value) in column.word_values(index, selected)? {
            let Some(value) = value else { continue };
            let value: i64 = value.into();
            let payload = payload_at(&mut access, region, layout, offsets[row])?;
            let flags = read_word(payload, slot.flags_at);
            let state = read_word(payload, slot.value_at) as i64;
            let next = if flags & flag == 0 {
                value
            } else {
                match fold {
                    Fold::Sum => state
                        .checked_add(value)
                        .ok_or(ArithmeticError::BigintOutOfRange)?,
                    Fold::Min => state.min(value),
                    Fold::Max => state.max(value),
                }
            };
            write_word(payload, slot.value_at, next as u64);
            write_word(payload, slot.flags_at, flags | flag);
        }
    }
    Ok(())
}
