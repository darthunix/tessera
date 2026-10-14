//! What one writer alone may do: find or create the record of a key,
//! link a chunk's records next to those of the same key, change payloads,
//! walk the records and move the table to a larger index.
//!
//! These need the table to itself: a record found may be updated in place,
//! a walk reads every chunk up to its used mark, which its writer moves,
//! and a larger index rebuilds the buckets and every record's next field.
//! Records never move.

use anyhow::{Result, bail, ensure};
use tessera_core::{RowMask, RowMaskView, ones};

use super::batch::{Lanes, VERTICAL_MIN_ROWS, check_keys, check_partitions, probe_word, shaped};
use super::batch::{check_record, unlinked};
use super::header::{CHUNK_HEADER, Header, KEY_SLOT, Layout, RECORD_HEADER, placement};
use super::keys::{KeySource, WordKeys, slot_buffer};
use super::record::{Access, same_keys};
use super::region::{RawRegion, Region};
use super::{Batch, Partitions, Table};

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
    pub(super) fn at(chunk: usize, byte: usize) -> Self {
        Self((chunk as u64) << 32 | byte as u64)
    }

    pub(super) fn chunk(self) -> usize {
        (self.0 >> 32) as usize
    }

    pub(super) fn byte(self) -> usize {
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
        let selected = pending.as_view().word(index);
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
            // The walk of `ones` written out: through the iterator this
            // loop took an instruction more; `.cargo/mutants.toml` leaves
            // the step out.
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

/// Hand `each` the payload of each selected row's record, row by row, to
/// change in place: one access to the table for a batch, as the folds
/// make. Two rows of one record get the same payload, one after the
/// other; a reference that names no record fails the call there.
pub(super) fn each_payload<R: Region>(
    region: &R,
    layout: &Layout,
    offsets: &[u32],
    rows: &RowMaskView<'_>,
    mut each: impl FnMut(usize, &mut [u8]),
) -> Result<()> {
    let nrows = rows.nrows();
    ensure!(
        offsets.len() == nrows,
        "the offsets and mask of the batch have different row counts"
    );
    let mut access = Access::new(region, layout);
    for index in 0..nrows.div_ceil(64) {
        for bit in ones(rows.word(index)) {
            let row = index * 64 + bit;
            each(row, payload_at(&mut access, region, layout, offsets[row])?);
        }
    }
    Ok(())
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

/// The one writer's cursor over the payloads of its table, made once a
/// call by [`TableMut::payloads`](super::TableMut::payloads): it gives the payload of the record a
/// reference names, to change in place, after checking the reference as
/// a lookup checks it. Each payload borrows the cursor, so two are never
/// open at once, even of one record. It also holds up to [`HELD`]
/// references it has checked, whose payloads open later without a second
/// check ([`Self::hold`], [`Self::held`]); what it holds never leaves it.
pub struct PayloadCursor<'t> {
    access: Access<'t, RawRegion>,
    held: [*mut u8; HELD],
}

/// The references a [`PayloadCursor`] holds at once.
pub const HELD: usize = 16;

impl<'t> PayloadCursor<'t> {
    /// A cursor over the payloads of `table`, which its writer lends it.
    #[inline(always)]
    pub(super) fn new(table: &'t mut Table<'_>) -> Self {
        Self {
            access: Access::new(&table.region, &table.layout),
            held: [core::ptr::null_mut(); HELD],
        }
    }

    /// The payload of the record at `offset`, to change in place: an
    /// error for a reference that names no record of the table.
    #[inline(always)]
    pub fn payload(&mut self, offset: u32) -> Result<&mut [u8]> {
        let spot = self.access.payload_spot(offset)?;
        // SAFETY: the spot is this record's payload, just checked; the
        // slice borrows the cursor, which borrows the writer's table, so
        // no other slice of a payload is alive while it is.
        Ok(unsafe { self.access.payload_mut(spot) })
    }

    /// Check the reference `offset` now, as [`Self::payload`] does, and
    /// hold its payload's place in `slot`, below [`HELD`], for
    /// [`Self::held`] to open later; a slot holds one place at a time.
    #[inline(always)]
    pub fn hold(&mut self, slot: usize, offset: u32) -> Result<()> {
        let spot = self.access.payload_spot(offset)?;
        let Some(held) = self.held.get_mut(slot) else {
            bail!("slot {slot} of a payload cursor's {HELD}");
        };
        *held = spot;
        Ok(())
    }

    /// The payload whose reference [`Self::hold`] checked into `slot`, to
    /// change in place: an error for a slot that holds none.
    #[inline(always)]
    pub fn held(&mut self, slot: usize) -> Result<&mut [u8]> {
        let Some(&spot) = self.held.get(slot).filter(|spot| !spot.is_null()) else {
            bail!("slot {slot} of a payload cursor holds no reference");
        };
        // SAFETY: the place was checked by `hold` over this cursor's table,
        // which the cursor borrows, so the record is still there; the
        // slice borrows the cursor, so no other slice of a payload lives.
        Ok(unsafe { self.access.payload_mut(spot) })
    }
}

/// Where [`TableMut::merge`](super::TableMut::merge) stopped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CombineStop {
    /// Every record of the source was merged.
    Done,
    /// A record of a new group needs another chunk.
    ChunkFull,
    /// A record of a new group needs a larger index.
    IndexFull,
}

/// Merge the records of chunk `source` from byte `*from` on into the
/// table by `rule`: a record whose keys the table has passes its payload
/// to `rule` with the payload of the table's record, which the rule
/// changes; a record of new keys is copied whole to chunk `chunk` and
/// linked. `*from` moves past the records merged; the call stops where a
/// record of new keys finds no room in `chunk` or the index holds as many
/// records as half its buckets. The count merged is returned. When the
/// rule fails, its error is returned, the records before it merged. The
/// source is none of the table's linked chunks, and a record found in it
/// is a damaged table: the rule's two payloads never share bytes.
pub(super) fn merge<R: Region>(
    region: &R,
    layout: &Layout,
    source: usize,
    from: &mut usize,
    chunk: usize,
    mut rule: impl FnMut(&[u8], &mut [u8]) -> Result<()>,
) -> Result<(usize, CombineStop)> {
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
            ensure!(
                placement(same).0 != source,
                "table record {same} found in chunk {source}, which is merged"
            );
            let payload = payload_at(&mut access, region, layout, same)?;
            if let Err(error) = rule(record.payload(), payload) {
                // SAFETY: as below.
                unsafe { access.set_used(chunk, used) };
                return Err(error);
            }
        }
        *from = byte + record_size;
        merged += 1;
    }
    // SAFETY: this is the chunk's writer, and `used` ends its records.
    unsafe { access.set_used(chunk, used) };
    Ok((merged, stop))
}
