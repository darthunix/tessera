//! What one writer alone may do: find or create the record of a key,
//! link a chunk's records next to those of the same key, change payloads,
//! walk the records and move the table to a larger index.
//!
//! These need the table to itself: a record found may be updated in place,
//! a walk reads every chunk up to its used mark, which its writer moves,
//! and a larger index rebuilds the buckets and every record's next field.
//! Records never move.

use anyhow::{Result, bail, ensure};
use tessera_core::{ColumnReader, RowMask, RowMaskView};

use crate::decimal::{self, ExtremeState, Offer, Partial, Partials, SumState, Term, Terms};
use crate::ops::ArithmeticError;

use super::batch::{Lanes, VERTICAL_MIN_ROWS, check_keys, check_partitions, probe_word, shaped};
use super::batch::{check_record, unlinked};
use super::header::{CHUNK_HEADER, Header, KEY_SLOT, Layout, RECORD_HEADER};
use super::keys::{KeySource, WordKeys, slot_buffer};
use super::record::{Access, same_keys};
use super::region::Region;
use super::{Batch, Partitions};

/// Where a walk over the records stands: the chunk of the next record to
/// visit in the high 32 bits, its byte there in the low ones.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Cursor(u64);

impl Cursor {
    /// Before the first record.
    pub fn start() -> Self {
        Self::at(0, CHUNK_HEADER)
    }

    /// At byte `byte` of chunk `chunk`.
    fn at(chunk: usize, byte: usize) -> Self {
        Self((chunk as u64) << 32 | byte as u64)
    }

    fn chunk(self) -> usize {
        (self.0 >> 32) as usize
    }

    fn byte(self) -> usize {
        (self.0 & u64::from(u32::MAX)) as usize
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

/// Give each pending row of `batch` the record of its keys, creating one
/// with a zero payload in chunk `chunk` where none exists, until the chunk
/// has no room for a new one or the index holds as many records as half
/// its buckets: resolved rows leave the pending rows and get their
/// references, and the rows whose record this call created form
/// `inserted`.
pub(super) fn find_or_insert<R: Region, K: KeySource + ?Sized>(
    region: &R,
    layout: &Layout,
    chunk: usize,
    batch: &mut Batch<'_, '_, K>,
    inserted: &mut RowMask<'_>,
) -> Result<usize> {
    let Batch {
        hashes,
        keys,
        pending,
        offsets,
    } = batch;
    let nrows = offsets.len();
    check_keys(layout, *keys)?;
    ensure!(
        inserted.as_view().nrows() == nrows,
        "the inserted mask has {} rows, the batch {nrows}",
        inserted.as_view().nrows()
    );
    shaped!(
        layout.nkeys,
        layout.tail_words(),
        resolve_rows(
            region, layout, chunk, None, hashes, *keys, pending, offsets, inserted
        )
    )
}

/// As [`find_or_insert`], but a new record goes to the chunk of its
/// hash's partition, whose one writer the caller is: a row whose
/// partition's chunk is full stays pending while the rows after it go on,
/// and all stop once the index holds as many records as half its buckets.
pub(super) fn find_or_insert_partitioned<R: Region, K: KeySource + ?Sized>(
    region: &R,
    layout: &Layout,
    partitions: &Partitions<'_>,
    batch: &mut Batch<'_, '_, K>,
    inserted: &mut RowMask<'_>,
) -> Result<usize> {
    let Batch {
        hashes,
        keys,
        pending,
        offsets,
    } = batch;
    let nrows = offsets.len();
    check_keys(layout, *keys)?;
    ensure!(
        inserted.as_view().nrows() == nrows,
        "the inserted mask has {} rows, the batch {nrows}",
        inserted.as_view().nrows()
    );
    let mask = check_partitions(region, partitions)?;
    shaped!(
        layout.nkeys,
        layout.tail_words(),
        resolve_rows(
            region,
            layout,
            0,
            Some((partitions, mask)),
            hashes,
            *keys,
            pending,
            offsets,
            inserted
        )
    )
}

/// The rows of [`find_or_insert`] for a table of `N` keys and `T` words
/// after them, 0 for either when it is not one of the specialized shapes;
/// with partitions and the mask of a partition number, a new record goes
/// to its partition's chunk instead of `chunk`.
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
    partitions: Option<(&Partitions<'_>, u32)>,
    hashes: &[u32],
    keys: &K,
    pending: &mut RowMask<'_>,
    offsets: &mut [u32],
    inserted: &mut RowMask<'_>,
) -> Result<usize> {
    // Made here, not passed in, so that its fields stay in registers.
    let mut access = Access::new(region, layout);
    let (mut used, mut room) = match partitions {
        None => access.room(chunk)?,
        Some(_) => (0, 0),
    };
    // The index takes records up to half its buckets.
    let limit = u64::from(layout.nbuckets / 2);
    let nrows = pending.as_view().nrows();
    let mut buffer = slot_buffer::<L>();
    let mut word_keys = WordKeys::new(&mut buffer, access.nkeys());
    let mut lanes = Lanes::new();
    let mut resolved = 0;
    let mut full = false;
    for index in 0..nrows.div_ceil(64) {
        let selected = pending.as_view().word_at(index);
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
                        if access.records() >= limit {
                            full = true;
                            break;
                        }
                        let place = match partitions {
                            None if room == 0 => {
                                full = true;
                                break;
                            }
                            None => (chunk, used),
                            Some((partitions, mask)) => {
                                let target = partitions.chunks
                                    [((hash >> partitions.shift) & mask) as usize]
                                    as usize;
                                let (at, space) = access.room(target)?;
                                // The row waits for a new chunk of its partition.
                                if space == 0 {
                                    continue;
                                }
                                (target, at)
                            }
                        };
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
                        if partitions.is_none() {
                            used += access.record_size();
                            room -= 1;
                        } else {
                            // SAFETY: this is the chunk's writer, and the
                            // record just written ends at the new mark.
                            unsafe { access.set_used(place.0, place.1 + access.record_size()) };
                        }
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
    if partitions.is_none() {
        // SAFETY: this is the chunk's writer, and `used` ends its records.
        unsafe { access.set_used(chunk, used) };
    }
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
    Ok(unsafe { region.record_mut(region.spot(chunk, start), layout.payload_size) })
}

/// Write 0 into key slot `key` of every record, its NULL bit kept, and
/// return the count of records: the key then orders no two records that
/// are not NULL. For records nothing finds by their keys, such as a
/// sort's, which gives up a key's abbreviated values this way.
pub(super) fn clear_key<R: Region>(region: &R, layout: &Layout, key: usize) -> Result<u64> {
    ensure!(
        key < layout.nkeys,
        "key {key} of a table of {} keys",
        layout.nkeys
    );
    let access = Access::new(region, layout);
    let mut count = 0;
    for chunk in 0..region.chunks() {
        let (used, _) = access.room(chunk)?;
        let mut byte = CHUNK_HEADER;
        while byte < used {
            check_record(&access, (chunk, byte))?;
            let slot = byte + RECORD_HEADER + key * KEY_SLOT;
            // SAFETY: the caller has the table to itself, and the slot lies
            // within a record that `check_record` accepted.
            unsafe { region.record_mut(region.spot(chunk, slot), KEY_SLOT) }.fill(0);
            byte += layout.record_size;
            count += 1;
        }
    }
    Ok(count)
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
    let (mut chunk, mut byte) = (cursor.chunk(), cursor.byte());
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
    *cursor = Cursor::at(chunk, byte);
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
    let spot = payload_spot(access, region, layout, offset)?;
    // SAFETY: the spot is this record's payload, just checked; the slice is
    // dropped before the next row's is made.
    Ok(unsafe { payload_of(region, layout, spot) })
}

/// Where the payload of the record at `offset` starts, the record checked.
#[inline(always)]
fn payload_spot<'r, R: Region>(
    access: &mut Access<'r, R>,
    region: &'r R,
    layout: &Layout,
    offset: u32,
) -> Result<R::Spot> {
    access.locate(offset)?;
    let (chunk, byte) = access.place(offset)?;
    let start = byte + RECORD_HEADER + layout.nkeys * KEY_SLOT;
    // SAFETY: the payload lies within a record that `locate` accepted.
    Ok(unsafe { region.spot(chunk, start) })
}

/// The payload at a spot [`payload_spot`] gave, to change in place.
///
/// # Safety
///
/// The spot comes from [`payload_spot`] during this operation, the caller
/// has the table to itself, and no other slice of the same payload is
/// alive while this one is.
#[inline(always)]
#[allow(clippy::mut_from_ref)]
unsafe fn payload_of<'r, R: Region>(region: &'r R, layout: &Layout, spot: R::Spot) -> &'r mut [u8] {
    // SAFETY: the caller's contract.
    unsafe { region.record_mut(spot, layout.payload_size) }
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
        let mut bits = rows.word_at(index);
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
        let selected = rows.word_at(index);
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
        let selected = rows.word_at(index);
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

/// The most sums [`sum_terms`] folds in a call.
pub const MAX_SUMS: usize = 32;

/// A sum or an average whose [`SumState`] a record keeps at byte `at` of
/// its payload, the terms of its rows ([`Terms`], or their partial states,
/// [`Partials`]), and the mask of the rows it leaves to the caller.
pub struct SumSlot<'a, T: ?Sized> {
    /// The rows' terms or partial states.
    pub terms: &'a T,
    /// The state's first byte in the payload.
    pub at: usize,
    /// The rows the state does not take; every word is written.
    pub rest: RowMask<'a>,
}

/// Groups a word of rows folds first among themselves, before their
/// records: past them, a word's rows go to their records one by one.
const LOCAL_GROUPS: usize = 16;

/// Slots of the hash of a word's groups: twice the groups, so that a probe
/// ends.
const LOCAL_SLOTS: usize = 2 * LOCAL_GROUPS;

/// The decimals one group's rows of a word gave one sum, added up: their
/// sum at their scale and how many.
#[derive(Clone, Copy, Debug, Default)]
struct Local {
    value: i128,
    count: u64,
    scale: u32,
}

/// The group of a word's record `offset`, a new one while there is room;
/// `None` once the word has [`LOCAL_GROUPS`] groups and this is another.
#[inline(always)]
fn local_group(
    slots: &mut [u8; LOCAL_SLOTS],
    groups: &mut [u32; LOCAL_GROUPS],
    ngroups: &mut usize,
    offset: u32,
) -> Option<usize> {
    let mut slot =
        (offset.wrapping_mul(0x9E37_79B1) >> (32 - LOCAL_SLOTS.trailing_zeros())) as usize;
    loop {
        let entry = slots[slot];
        if entry == 0 {
            if *ngroups == LOCAL_GROUPS {
                return None;
            }
            groups[*ngroups] = offset;
            *ngroups += 1;
            slots[slot] = *ngroups as u8;
            return Some(*ngroups - 1);
        }
        if groups[usize::from(entry) - 1] == offset {
            return Some(usize::from(entry) - 1);
        }
        slot = (slot + 1) % LOCAL_SLOTS;
    }
}

/// Fold one term into the [`SumState`] at byte `at` of a payload: false
/// when the state does not take it.
#[inline(always)]
fn fold_term(payload: &mut [u8], at: usize, term: Term) -> Result<bool> {
    // One check for the four words, then words at fixed places.
    let slots: &mut [u8; 8 * SumState::WORDS] =
        (&mut payload[at..at + 8 * SumState::WORDS]).try_into()?;
    let mut words: [u64; SumState::WORDS] = std::array::from_fn(|word| read_word(slots, 8 * word));
    let taken = SumState::add_to(&mut words, term);
    if taken {
        for (word, value) in words.into_iter().enumerate() {
            write_word(slots, 8 * word, value);
        }
    }
    Ok(taken)
}

/// Fold each selected row's terms into the [`SumState`]s of its record's
/// payload, as if row by row: a decimal into the sum, NaN or an infinity
/// into its flag, NULL skipped. A row a state does not take (a longer
/// value, a decimal the sum refuses) goes to that sum's rest, for the
/// caller to add by the core's means.
///
/// A word's rows are first added up by group, for up to [`LOCAL_GROUPS`]
/// groups a word, whose records are found once: for each sum, the decimals
/// of one scale a group's rows give make one local sum, which goes into the
/// record once. The rows a source hands over in bulk (decimals of one scale
/// without NULL, integers) are added by a loop over their values alone; the
/// others go term by term, and a term that is no such decimal goes to the
/// record directly, as do the rows of further groups. Exact sums add in any
/// order; a local sum the state refuses at its bound is taken again row by
/// row, so that the rows it refuses go to the rest.
pub(super) fn sum_terms<R: Region, T: Terms>(
    region: &R,
    layout: &Layout,
    offsets: &[u32],
    rows: &RowMaskView<'_>,
    sums: &mut [SumSlot<'_, T>],
) -> Result<()> {
    let nrows = rows.nrows();
    let nsums = sums.len();
    ensure!(nsums <= MAX_SUMS, "{nsums} sums in a call, past {MAX_SUMS}");
    ensure!(
        offsets.len() == nrows,
        "the offsets and mask of the batch have different row counts"
    );
    for sum in sums.iter() {
        let words: [usize; SumState::WORDS] = std::array::from_fn(|word| sum.at + 8 * word);
        check_accumulate(layout, offsets.len(), nrows, &words)?;
        ensure!(
            sum.rest.as_view().nrows() == nrows,
            "the rest and the selection of a sum have different row counts"
        );
    }
    let mut access = Access::new(region, layout);
    let mut others = [0_u64; MAX_SUMS];
    let mut groups = [0_u32; LOCAL_GROUPS];
    let mut spots: [Option<R::Spot>; LOCAL_GROUPS] = [None; LOCAL_GROUPS];
    let mut locals = [Local::default(); LOCAL_GROUPS];
    let mut row_groups = [0_u8; 64];
    let mut group_rows = [0_u64; LOCAL_GROUPS];
    let mut bulk = [0_i128; LOCAL_GROUPS];
    for index in 0..nrows.div_ceil(64) {
        let selected = rows.word_at(index);
        // The word's groups, and the rows of the groups that fit.
        let mut slots = [0_u8; LOCAL_SLOTS];
        let mut ngroups = 0;
        let mut local_rows = 0_u64;
        let mut look = selected;
        while look != 0 {
            let bit = look.trailing_zeros() as usize;
            look &= look - 1;
            match local_group(
                &mut slots,
                &mut groups,
                &mut ngroups,
                offsets[index * 64 + bit],
            ) {
                Some(group) => {
                    row_groups[bit] = group as u8;
                    local_rows |= 1 << bit;
                }
                None => break,
            }
        }
        group_rows[..ngroups].fill(0);
        let mut look = local_rows;
        while look != 0 {
            let bit = look.trailing_zeros() as usize;
            look &= look - 1;
            group_rows[usize::from(row_groups[bit]) % LOCAL_GROUPS] |= 1 << bit;
        }
        for (spot, &offset) in spots.iter_mut().zip(&groups[..ngroups]) {
            *spot = Some(payload_spot(&mut access, region, layout, offset)?);
        }
        for (sum, other) in sums.iter().zip(others.iter_mut()) {
            *other = 0;
            locals[..ngroups].fill(Local::default());
            let mut taken = 0_u64;
            // The rows handed over in bulk: their values alone, added up by
            // group as the source reads them, counted by the groups' rows.
            // At most 64 values of an i64 each: no group's bulk sum comes
            // near the range of an i128, so it is added unchecked; the bound
            // of the state is checked when the sum is taken into it.
            bulk[..ngroups].fill(0);
            if let Some(word) = sum.terms.fold_decimals(index, local_rows, |bit, value| {
                bulk[usize::from(row_groups[bit % 64]) % LOCAL_GROUPS] += i128::from(value);
            }) {
                ensure!(
                    word.rows & !local_rows == 0 && word.scale <= decimal::MAX_READ_SCALE,
                    "a source handed over rows it was not asked for"
                );
                for ((local, &value), &rows) in locals[..ngroups]
                    .iter_mut()
                    .zip(&bulk[..ngroups])
                    .zip(&group_rows[..ngroups])
                {
                    *local = Local {
                        value,
                        count: u64::from((rows & word.rows).count_ones()),
                        scale: word.scale,
                    };
                }
                taken = word.rows;
            }
            // The other rows of the word's groups, term by term.
            let mut look = local_rows & !taken;
            while look != 0 {
                let bit = look.trailing_zeros() as usize;
                look &= look - 1;
                let group = usize::from(row_groups[bit]);
                let term = sum.terms.term(index * 64 + bit);
                let local = &mut locals[group];
                match term {
                    Term::Null => {}
                    Term::Decimal(decimal)
                        if decimal.scale() <= decimal::MAX_READ_SCALE
                            && (local.count == 0 || local.scale == decimal.scale()) =>
                    {
                        local.value += i128::from(decimal.value());
                        local.count += 1;
                        local.scale = decimal.scale();
                        taken |= 1 << bit;
                    }
                    _ => {
                        let Some(spot) = spots[group] else {
                            bail!("a group without the record it was found in");
                        };
                        // SAFETY: the group's spot was found in this call;
                        // the slice goes before another is made.
                        let payload = unsafe { payload_of(region, layout, spot) };
                        if !fold_term(payload, sum.at, term)? {
                            *other |= 1 << bit;
                        }
                    }
                }
            }
            // Each group's local sum into its record.
            for (group, local) in locals[..ngroups].iter().enumerate() {
                if local.count == 0 {
                    continue;
                }
                let Some(spot) = spots[group] else {
                    bail!("a group without the record it was found in");
                };
                // SAFETY: as above.
                let payload = unsafe { payload_of(region, layout, spot) };
                let slots: &mut [u8; 8 * SumState::WORDS] =
                    (&mut payload[sum.at..sum.at + 8 * SumState::WORDS]).try_into()?;
                let mut words: [u64; SumState::WORDS] =
                    std::array::from_fn(|word| read_word(slots, 8 * word));
                if SumState::add_many_to(&mut words, local.value, local.scale, local.count) {
                    for (word, value) in words.into_iter().enumerate() {
                        write_word(slots, 8 * word, value);
                    }
                    continue;
                }
                // At the bound: the rows that made the local sum, one by one;
                // one that is no decimal of its scale goes to the rest.
                let mut again = taken;
                while again != 0 {
                    let bit = again.trailing_zeros() as usize;
                    again &= again - 1;
                    if usize::from(row_groups[bit]) != group {
                        continue;
                    }
                    let term = sum.terms.term(index * 64 + bit);
                    let decimal =
                        matches!(term, Term::Decimal(decimal) if decimal.scale() == local.scale);
                    if !decimal || !fold_term(payload, sum.at, term)? {
                        *other |= 1 << bit;
                    }
                }
            }
        }
        // The rows of further groups, each to its record.
        let mut look = selected & !local_rows;
        while look != 0 {
            let bit = look.trailing_zeros() as usize;
            look &= look - 1;
            let row = index * 64 + bit;
            let payload = payload_at(&mut access, region, layout, offsets[row])?;
            for (sum, other) in sums.iter().zip(others.iter_mut()) {
                let term = sum.terms.term(row);
                if !matches!(term, Term::Null) && !fold_term(payload, sum.at, term)? {
                    *other |= 1 << bit;
                }
            }
        }
        for (sum, &other) in sums.iter_mut().zip(others.iter()) {
            sum.rest.set_word(index, other)?;
        }
    }
    Ok(())
}

/// Merge each selected row's partial states into the [`SumState`]s of its
/// record's payload ([`SumState::merge`]), the record found once a row:
/// NULL skipped, a state left to the caller ([`Partial::Other`]) or one
/// whose sum the record's refuses at its bound set in that sum's rest, the
/// record's state unchanged. A final grouping's rows come a group's partial
/// state from each participant, the groups of a batch from one: its rows
/// are not added up by group first, as [`sum_terms`] adds up terms.
pub(super) fn sum_partials<R: Region, P: Partials>(
    region: &R,
    layout: &Layout,
    offsets: &[u32],
    rows: &RowMaskView<'_>,
    sums: &mut [SumSlot<'_, P>],
) -> Result<()> {
    let nrows = rows.nrows();
    let nsums = sums.len();
    ensure!(nsums <= MAX_SUMS, "{nsums} sums in a call, past {MAX_SUMS}");
    for sum in sums.iter() {
        let words: [usize; SumState::WORDS] = std::array::from_fn(|word| sum.at + 8 * word);
        check_accumulate(layout, offsets.len(), nrows, &words)?;
        ensure!(
            sum.rest.as_view().nrows() == nrows,
            "the rest and the selection of a sum have different row counts"
        );
    }
    let mut access = Access::new(region, layout);
    let mut others = [0_u64; MAX_SUMS];
    for index in 0..nrows.div_ceil(64) {
        others[..nsums].fill(0);
        let mut look = rows.word_at(index);
        while look != 0 {
            let bit = look.trailing_zeros() as usize;
            look &= look - 1;
            let row = index * 64 + bit;
            let payload = payload_at(&mut access, region, layout, offsets[row])?;
            for (sum, other) in sums.iter().zip(others.iter_mut()) {
                let state = match sum.terms.partial(row)? {
                    Partial::Null => continue,
                    Partial::State(state) => state,
                    Partial::Other => {
                        *other |= 1 << bit;
                        continue;
                    }
                };
                let slots: &mut [u8; 8 * SumState::WORDS] =
                    (&mut payload[sum.at..sum.at + 8 * SumState::WORDS]).try_into()?;
                let mut ours =
                    SumState::from_words(std::array::from_fn(|word| read_word(slots, 8 * word)));
                if !ours.merge(state) {
                    *other |= 1 << bit;
                    continue;
                }
                for (word, value) in ours.to_words().into_iter().enumerate() {
                    write_word(slots, 8 * word, value);
                }
            }
        }
        for (sum, &other) in sums.iter_mut().zip(others.iter()) {
            sum.rest.set_word(index, other)?;
        }
    }
    Ok(())
}

/// A `min` or `max` of numeric of [`extremes`]: its rows' terms, its
/// state in the payload, and the rows it leaves to the caller.
pub struct ExtremeSlot<'a, T: ?Sized> {
    /// The rows' terms.
    pub terms: &'a T,
    /// The state's first byte in the payload ([`ExtremeState`]).
    pub at: usize,
    /// `max`, or `min` when false.
    pub max: bool,
    /// The rows the state leaves to the caller; every word is written.
    pub rest: RowMask<'a>,
}

/// Offer each selected row's term to the `min` or `max` state of its
/// record's payload ([`ExtremeState::offer`]), in the rows' order: the
/// rows the state does not decide go to the slot's rest, and the group's
/// later rows of the batch after them, for the caller to take in order.
pub(super) fn extremes<R: Region, T: Terms>(
    region: &R,
    layout: &Layout,
    offsets: &[u32],
    rows: &RowMaskView<'_>,
    slot: &mut ExtremeSlot<'_, T>,
) -> Result<()> {
    let nrows = rows.nrows();
    let words: [usize; ExtremeState::WORDS] = std::array::from_fn(|word| slot.at + 8 * word);
    check_accumulate(layout, offsets.len(), nrows, &words)?;
    ensure!(
        slot.rest.as_view().nrows() == nrows,
        "the rest and the selection of an extreme have different row counts"
    );
    let mut access = Access::new(region, layout);
    for index in 0..nrows.div_ceil(64) {
        let mut other = 0_u64;
        let mut look = rows.word_at(index);
        while look != 0 {
            let bit = look.trailing_zeros() as usize;
            look &= look - 1;
            let row = index * 64 + bit;
            let term = slot.terms.term(row);
            if term == Term::Null {
                continue;
            }
            let payload = payload_at(&mut access, region, layout, offsets[row])?;
            let (value, flags) = (read_word(payload, slot.at), read_word(payload, slot.at + 8));
            let mut state = ExtremeState::from_words(value, flags)?;
            match state.offer(term, slot.max) {
                Offer::Kept => continue,
                Offer::Taken => {}
                Offer::Rest => other |= 1 << bit,
            }
            let (value, flags) = state.to_words();
            write_word(payload, slot.at, value);
            write_word(payload, slot.at + 8, flags);
        }
        slot.rest.set_word(index, other)?;
    }
    Ok(())
}

/// How an aggregate's state of a record merges into the state of the same
/// group in another: the payload holds a word of flags, bit `i` set once
/// aggregate `i` has a value, then a word per aggregate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Combine {
    /// A count: the two add, an overflow failing as the transition would.
    Count,
    /// A sum: the two add when both have a value, else the one that has.
    Sum,
    /// The least of the two values.
    Min,
    /// The greatest of the two values.
    Max,
}

/// Where [`combine`] stopped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CombineStop {
    /// Every record of the source was merged.
    Done,
    /// A record of a new group needs another chunk.
    ChunkFull,
    /// A record of a new group needs a larger index.
    IndexFull,
}

/// Merge the records of chunk `source` from byte `*from` on, each the
/// states of a group, into the table: the record of the same keys takes
/// the states in, and a group the table lacks is copied whole to chunk
/// `chunk` and linked. `*from` moves past the records merged; the call
/// stops where a new group finds no room in `chunk` or the index holds as
/// many records as half its buckets. The count merged is returned. The
/// source is none of the table's linked chunks.
pub(super) fn combine<R: Region>(
    region: &R,
    layout: &Layout,
    source: usize,
    from: &mut usize,
    chunk: usize,
    combines: &[Combine],
) -> Result<(usize, CombineStop)> {
    ensure!(
        combines.len() <= 64 && 8 * (1 + combines.len()) <= layout.payload_size,
        "{} aggregates do not fit a payload of {} bytes with a word of flags",
        combines.len(),
        layout.payload_size
    );
    ensure!(
        source != chunk,
        "table chunk {source} is merged into itself"
    );
    let mut access = Access::new(region, layout);
    let record_size = access.record_size();
    let limit = u64::from(layout.nbuckets / 2);
    let (mut used, mut room) = access.room(chunk)?;
    let places = unlinked(&access, source, *from)?;
    let mut merged = 0;
    let mut stop = CombineStop::Done;
    let mut words = [0u64; 65];
    for byte in places {
        let hash = check_record(&access, (source, byte))?;
        // SAFETY: `check_record` accepted the record, which nothing writes.
        let record = unsafe { access.view((source, byte)) };
        let (null_bits, keys) = (record.null_bits(), record.keys());
        let head = access.head(hash);
        let same = access.find(head, hash, |other| {
            other.null_bits() == null_bits && same_keys(other.keys(), keys)
        })?;
        if same == 0 {
            if room == 0 {
                stop = CombineStop::ChunkFull;
                break;
            }
            if access.records() >= limit {
                stop = CombineStop::IndexFull;
                break;
            }
            let place = (chunk, used);
            // SAFETY: the source record lies below its chunk's used mark,
            // the copy past the destination's, within it as `room`
            // counted, and the chunks differ; this is the table's writer.
            unsafe {
                let bytes = region.record(region.spot(source, byte), record_size);
                let copy = region.record_mut(region.spot(chunk, used), record_size);
                copy.copy_from_slice(bytes);
            }
            let offset = access.reference(place);
            access.count(1);
            // SAFETY: the record was just written and is this writer's.
            unsafe { access.push(offset, place, hash) };
            used += record_size;
            room -= 1;
        } else {
            let incoming = record.payload();
            for (index, word) in words.iter_mut().enumerate().take(1 + combines.len()) {
                *word = read_word(incoming, 8 * index);
            }
            let payload = payload_at(&mut access, region, layout, same)?;
            let mut flags = read_word(payload, 0);
            for (index, &how) in combines.iter().enumerate() {
                let at = 8 * (1 + index);
                let flag = 1u64 << index;
                let theirs = words[1 + index] as i64;
                let ours = read_word(payload, at) as i64;
                let next = match how {
                    Combine::Count => ours
                        .checked_add(theirs)
                        .ok_or(ArithmeticError::BigintOutOfRange)?,
                    _ if words[0] & flag == 0 => continue,
                    _ if flags & flag == 0 => theirs,
                    Combine::Sum => ours
                        .checked_add(theirs)
                        .ok_or(ArithmeticError::BigintOutOfRange)?,
                    Combine::Min => ours.min(theirs),
                    Combine::Max => ours.max(theirs),
                };
                if how != Combine::Count {
                    flags |= flag;
                }
                write_word(payload, at, next as u64);
            }
            write_word(payload, 0, flags);
        }
        *from = byte + record_size;
        merged += 1;
    }
    // SAFETY: this is the chunk's writer, and `used` ends its records.
    unsafe { access.set_used(chunk, used) };
    Ok((merged, stop))
}
