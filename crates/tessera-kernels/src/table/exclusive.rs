//! What one writer alone may do: find or create the record of a key,
//! change payloads, walk the records and grow the region.
//!
//! These need the region to itself: a record found may be updated in
//! place, a walk reads every record below the used mark, which an
//! insertion in flight would have reserved but not written, and growth
//! rewrites the header and the buckets.

use anyhow::{Result, ensure};
use tessera_core::RowMask;

use super::batch::{check, shaped};
use super::header::{CHUNK_USED, HEADER_SIZE, Header, KEY_SLOT, Layout, RECORD_HEADER};
use super::keys::{KeySource, WordKeys, slot_buffer};
use super::record::Access;
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
fn resolve_rows<R: Region, K: KeySource + ?Sized, const N: usize, const T: usize>(
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
    let mut buffer = slot_buffer();
    let mut word_keys = WordKeys::new(&mut buffer, access.nkeys());
    let mut resolved = 0;
    let mut full = false;
    for index in 0..nrows.div_ceil(64) {
        let selected = pending.as_view().word(index).unwrap();
        let mut done = 0;
        let mut created = 0;
        if selected != 0 && !full {
            word_keys.load(keys, index, selected)?;
            let mut bits = selected;
            while bits != 0 {
                let bit = bits.trailing_zeros() as usize;
                bits &= bits - 1;
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
    unsafe { region.bytes_mut(grown.buckets_offset, grown.nbuckets as usize * 4) }.fill(0);
    let used = region.load_u64(CHUNK_USED) as usize;
    let mut access = Access::new(region, &grown);
    let mut byte = HEADER_SIZE;
    while byte < used {
        let offset = (byte / 8) as u32;
        let record = access.locate(offset)?;
        let (hash, len) = (record.hash(), record.len());
        access.push(offset, byte, hash);
        byte += len;
    }
    Ok(grown)
}
