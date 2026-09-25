//! What one writer alone may do: find or create the record of a key,
//! link a chunk's records next to those of the same key, change payloads,
//! walk the records and move the table to a larger index.
//!
//! These need the table to itself: a record found may be updated in place,
//! a walk reads every chunk up to its used mark, which its writer moves,
//! and a larger index rebuilds the buckets and every record's next field.
//! Records never move.

use anyhow::{Result, ensure};
use tessera_core::{ColumnReader, RowMask, RowMaskView};

use crate::ops::ArithmeticError;

use super::batch::{Lanes, VERTICAL_MIN_ROWS, check, probe_word, shaped};
use super::batch::{check_record, unlinked};
use super::header::{CHUNK_HEADER, Header, KEY_SLOT, Layout, RECORD_HEADER};
use super::keys::{KeySource, WordKeys, slot_buffer};
use super::record::{Access, same_keys};
use super::region::Region;

/// Where a walk over the records stands: the chunk of the next record to
/// visit in the high 32 bits, its byte there in the low ones.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Cursor(u64);

impl Cursor {
    /// Before the first record.
    pub fn start() -> Self {
        Self(CHUNK_HEADER as u64)
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
/// zero payload in chunk `chunk` where none exists, until the chunk has no
/// room for a new one or the index holds as many records as half its
/// buckets: resolved rows leave `pending`, get their references in
/// `offsets`, and the rows whose record this call created form `inserted`.
#[allow(clippy::too_many_arguments)]
pub(super) fn find_or_insert<R: Region, K: KeySource + ?Sized>(
    region: &R,
    layout: &Layout,
    chunk: usize,
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
        resolve_rows(
            region, layout, chunk, hashes, keys, pending, offsets, inserted
        )
    )
}

/// The rows of [`find_or_insert`] for a table of `N` keys and `T` words
/// after them, 0 for either when it is not one of the specialized shapes.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn resolve_rows<
    R: Region,
    K: KeySource + ?Sized,
    const N: usize,
    const T: usize,
    const L: usize,
>(
    region: &R,
    layout: &Layout,
    chunk: usize,
    hashes: &[u32],
    keys: &K,
    pending: &mut RowMask<'_>,
    offsets: &mut [u32],
    inserted: &mut RowMask<'_>,
) -> Result<usize> {
    // Made here, not passed in, so that its fields stay in registers.
    let mut access = Access::new(region, layout);
    let (mut used, mut room) = access.room(chunk)?;
    // The index takes records up to half its buckets.
    let limit = u64::from(layout.nbuckets / 2);
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
                        if room == 0 || access.records() >= limit {
                            full = true;
                            break;
                        }
                        let place = (chunk, used);
                        let offset = access.reference(place);
                        // SAFETY: the place lies past the used mark and
                        // within the chunk, whose writer this is; the
                        // buffer and the shape are this table's.
                        unsafe { access.write::<N, T>(place, hash, &word_keys, bit, None) };
                        // Counted at once: a lookup of a later row may walk
                        // this record, and a walk longer than the count is
                        // corrupt.
                        access.count(1);
                        // SAFETY: the record was just written and is this
                        // writer's alone.
                        unsafe { access.push(offset, place, hash) };
                        used += access.record_size();
                        room -= 1;
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
    // SAFETY: this is the chunk's writer, and `used` ends its records.
    unsafe { access.set_used(chunk, used) };
    Ok(resolved)
}

/// Link the records of chunk `chunk` from byte `*from` to its used mark
/// into their buckets, each right after a record with the same keys when
/// the table holds one, so that the records of a key lie next to each other
/// in their chain and [`super::batch::next_in_group`] steps through them;
/// `*from` moves past them. Returns the records linked and, of them, those
/// whose keys the table already held.
pub(super) fn link_grouped<R: Region>(
    region: &R,
    layout: &Layout,
    chunk: usize,
    from: &mut usize,
) -> Result<(usize, usize)> {
    let mut access = Access::new(region, layout);
    let bytes = unlinked(&access, chunk, *from)?;
    let count = bytes.len();
    let mut duplicates = 0;
    for byte in bytes {
        let place = (chunk, byte);
        let hash = check_record(&access, place)?;
        // SAFETY: `check_record` accepted the record, which this writer
        // appended and nothing else writes.
        let record = unsafe { access.view(place) };
        let (null_bits, keys) = (record.null_bits(), record.keys());
        let head = access.head(hash);
        let same = access.find(head, hash, |other| {
            other.null_bits() == null_bits && same_keys(other.keys(), keys)
        })?;
        let offset = access.reference(place);
        // Counted at once: a later record's lookup may walk this one.
        access.count(1);
        if same == 0 {
            // SAFETY: the writer has the table to itself.
            unsafe { access.push(offset, place, hash) };
        } else {
            // SAFETY: the writer has the table to itself; `same` was
            // located by the walk just above.
            unsafe { access.link_after(same, place, offset) };
            duplicates += 1;
        }
        *from = byte + access.record_size();
    }
    Ok((count, duplicates))
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
    let mut access = Access::new(region, layout);
    access.locate(offset)?;
    let (chunk, byte) = access.place(offset)?;
    let start = byte + RECORD_HEADER + layout.nkeys * KEY_SLOT;
    // SAFETY: the caller has the table to itself, and the payload lies
    // within a record that `locate` accepted.
    Ok(unsafe { region.record_mut(chunk, start, layout.payload_size) })
}

/// Visit the records from `cursor` on, chunk by chunk and in the order
/// they were appended, as many as `out` holds; the count visited is
/// returned and the cursor moves past them.
pub(super) fn scan<R: Region>(
    region: &R,
    layout: &Layout,
    cursor: &mut Cursor,
    out: &mut [u32],
) -> Result<usize> {
    let access = Access::new(region, layout);
    let mut chunk = (cursor.0 >> 32) as usize;
    let mut byte = (cursor.0 & u64::from(u32::MAX)) as usize;
    ensure!(
        byte >= CHUNK_HEADER
            && (byte - CHUNK_HEADER).is_multiple_of(layout.record_size)
            && chunk <= region.chunks(),
        "table cursor {:#x} lies outside the records",
        cursor.0
    );
    let mut count = 0;
    while count < out.len() && chunk < region.chunks() {
        let (used, _) = access.room(chunk)?;
        if byte >= used {
            chunk += 1;
            byte = CHUNK_HEADER;
            continue;
        }
        check_record(&access, (chunk, byte))?;
        out[count] = access.reference((chunk, byte));
        count += 1;
        byte += layout.record_size;
    }
    cursor.0 = ((chunk as u64) << 32) | byte as u64;
    Ok(count)
}

/// Move the table to the index `to`, of `to.len()` bytes and its buckets
/// for `capacity` records, over the same chunks: the header keeps the
/// layout, and every record is linked again, after an earlier one with the
/// same keys when there is one, so that grouped chains stay grouped. The
/// records stay where they are. The new layout is returned.
pub(super) fn regrow<R: Region>(
    from: &R,
    to: &R,
    layout: &Layout,
    capacity: u64,
) -> Result<Layout> {
    let header = Header::load(from).regrown(capacity, to.len())?;
    let grown = header.validate(to.len())?;
    ensure!(
        grown.record_size == layout.record_size && grown.nkeys == layout.nkeys,
        "the table changed while it grew"
    );
    header.store(to);
    // SAFETY: the caller has the new index to itself.
    unsafe { to.zero_u32(grown.buckets_offset, grown.nbuckets as usize) };
    let mut access = Access::new(to, &grown);
    for chunk in 0..to.chunks() {
        for byte in unlinked(&access, chunk, CHUNK_HEADER)? {
            let place = (chunk, byte);
            let hash = check_record(&access, place)?;
            // SAFETY: `check_record` accepted the record; the writer has
            // the table to itself.
            let record = unsafe { access.view(place) };
            let (null_bits, keys) = (record.null_bits(), record.keys());
            let head = access.head(hash);
            let same = access.find(head, hash, |other| {
                other.null_bits() == null_bits && same_keys(other.keys(), keys)
            })?;
            let offset = access.reference(place);
            access.count(1);
            if same == 0 {
                // SAFETY: the writer has the table to itself.
                unsafe { access.push(offset, place, hash) };
            } else {
                // SAFETY: as above; `same` was located just before.
                unsafe { access.link_after(same, place, offset) };
            }
        }
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
    let (chunk, byte) = access.place(offset)?;
    let start = byte + RECORD_HEADER + layout.nkeys * KEY_SLOT;
    // SAFETY: the caller has the table to itself, and the payload lies
    // within a record that `locate` accepted; the slice is dropped before
    // the next row's is made.
    Ok(unsafe { region.record_mut(chunk, start, layout.payload_size) })
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
