//! Records: how one lies in its chunk and how it is found, written and
//! published.
//!
//! A record starts with 16 bytes: its hash, the reference of the next
//! record of its bucket, the bits of its keys that are NULL and its length
//! in 8-byte units; then one 8-byte slot per key and the payload, rounded
//! up to 8. A reference is the record's chunk and its offset there (see
//! [`super::header::reference`]); 0 is none. A record is appended to a
//! chunk by the chunk's one writer, then published by a compare-and-swap of
//! its bucket's head, after which it never changes and never moves.

use anyhow::Result;

use super::header::{
    CHUNK_HEADER, KEY_SLOT, Layout, NRECORDS, RECORD_HEADER, placement, reference,
};
use super::keys::WordKeys;
use super::region::Region;

const HASH: usize = 0;
const NEXT: usize = 4;
const NULL_BITS: usize = 8;
const LEN: usize = 12;

/// A record as the table exposes it.
#[derive(Debug, Clone, Copy)]
pub struct Record<'a> {
    /// The hash it was inserted with.
    pub hash: u32,
    /// Bit `k` set: key `k` is NULL, and its slot holds 0.
    pub null_bits: u32,
    /// One slot per key, in key order.
    pub keys: &'a [i64],
    /// The payload, as many bytes as the table was created for.
    pub payload: &'a [u8],
}

/// A published record, cut into its parts once: the fields of its header
/// read from a fixed array, the key slots and the payload are slices.
pub(super) struct View<'r> {
    header: &'r [u8; RECORD_HEADER],
    keys: &'r [i64],
    payload: &'r [u8],
}

impl<'r> View<'r> {
    #[inline(always)]
    fn field(&self, at: usize) -> u32 {
        let h = self.header;
        u32::from_ne_bytes([h[at], h[at + 1], h[at + 2], h[at + 3]])
    }

    #[inline(always)]
    pub(super) fn hash(&self) -> u32 {
        self.field(HASH)
    }

    #[inline(always)]
    pub(super) fn next(&self) -> u32 {
        self.field(NEXT)
    }

    #[inline(always)]
    pub(super) fn null_bits(&self) -> u32 {
        self.field(NULL_BITS)
    }

    /// The length the record claims, in bytes.
    #[inline(always)]
    pub(super) fn len(&self) -> usize {
        self.field(LEN) as usize * 8
    }

    #[inline(always)]
    pub(super) fn keys(&self) -> &'r [i64] {
        self.keys
    }

    #[inline(always)]
    pub(super) fn payload(&self) -> &'r [u8] {
        self.payload
    }

    #[inline]
    pub(super) fn record(&self) -> Record<'r> {
        Record {
            hash: self.hash(),
            null_bits: self.null_bits(),
            keys: self.keys,
            payload: self.payload,
        }
    }
}

/// Whether two records hold the same key slots, compared slot by slot: a
/// slice comparison would call memcmp for the one or two slots a key has.
#[inline(always)]
pub(super) fn same_keys(a: &[i64], b: &[i64]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x == y)
}

/// Record-level access during one operation, with the counters as read at
/// its start.
///
/// The fields of the layout the loops need are copied in, not borrowed,
/// so that they stay in registers across the row loops instead of being
/// reloaded through a reference on every row.
pub(super) struct Access<'r, R> {
    region: &'r R,
    buckets_offset: usize,
    bucket_shift: u32,
    record_size: usize,
    payload_size: usize,
    nkeys: usize,
    nrecords: u64,
}

/// Where a record lies: its chunk and its first byte there.
pub(super) type Place = (usize, usize);

impl<'r, R: Region> Access<'r, R> {
    #[inline]
    pub(super) fn new(region: &'r R, layout: &Layout) -> Self {
        let mut access = Self {
            region,
            buckets_offset: layout.buckets_offset,
            bucket_shift: layout.bucket_shift,
            record_size: layout.record_size,
            payload_size: layout.payload_size,
            nkeys: layout.nkeys,
            nrecords: 0,
        };
        access.refresh();
        access
    }

    /// Access for appending to chunks alone, which reads nothing of the
    /// index: a shared build appends before any participant made one.
    #[inline]
    pub(super) fn for_chunks(region: &'r R, layout: &Layout) -> Self {
        Self {
            region,
            buckets_offset: layout.buckets_offset,
            bucket_shift: layout.bucket_shift,
            record_size: layout.record_size,
            payload_size: layout.payload_size,
            nkeys: layout.nkeys,
            nrecords: 0,
        }
    }

    /// Bytes of a record.
    #[inline(always)]
    pub(super) fn record_size(&self) -> usize {
        self.record_size
    }

    /// Read the record count again.
    #[inline]
    fn refresh(&mut self) {
        self.nrecords = self.region.load_u64(NRECORDS);
    }

    /// The record at `offset`, which must lie within a chunk past its used
    /// mark's word and claim the table's record length.
    #[inline(always)]
    pub(super) fn locate(&mut self, offset: u32) -> Result<View<'r>> {
        self.place(offset)?;
        // SAFETY: `place` just accepted the offset.
        unsafe { self.open(offset) }
    }

    /// Where the record at `offset` lies, which must be within its chunk
    /// past the used mark's word; the first half of [`Self::locate`], which
    /// reads nothing of the record, so that a caller can prefetch it.
    #[inline(always)]
    pub(super) fn place(&self, offset: u32) -> Result<Place> {
        let (chunk, byte) = placement(offset);
        if chunk >= self.region.chunks()
            || byte < CHUNK_HEADER
            || byte + self.record_size > self.region.chunk_len(chunk)
        {
            return Err(outside(offset));
        }
        Ok((chunk, byte))
    }

    /// The record at `offset`, which must claim the table's record length;
    /// the second half of [`Self::locate`].
    ///
    /// # Safety
    ///
    /// [`Self::place`] accepted `offset` during this operation.
    #[inline(always)]
    pub(super) unsafe fn open(&self, offset: u32) -> Result<View<'r>> {
        // SAFETY: `place` accepted the offset, the caller promises.
        unsafe { self.open_at(self.spot(placement(offset)), offset) }
    }

    /// The record at `offset`, whose spot the caller resolved, as
    /// [`Self::open`] gives it.
    ///
    /// # Safety
    ///
    /// [`Self::place`] accepted `offset` during this operation, and `spot`
    /// is the spot of that place.
    #[inline(always)]
    pub(super) unsafe fn open_at(&self, spot: R::Spot, offset: u32) -> Result<View<'r>> {
        // SAFETY: the caller's contract.
        let view = unsafe { self.view_at(spot) };
        if view.len() != self.record_size {
            return Err(misplaced(offset));
        }
        Ok(view)
    }

    /// The spot of a place.
    ///
    /// # Safety
    ///
    /// [`Self::place`] accepted the place, or it lies below its chunk's
    /// used mark.
    #[inline(always)]
    pub(super) unsafe fn spot(&self, (chunk, byte): Place) -> R::Spot {
        // SAFETY: the caller's contract.
        unsafe { self.region.spot(chunk, byte) }
    }

    /// Hint that the bucket of a hash will be read soon.
    #[inline(always)]
    pub(super) fn prefetch_bucket(&self, hash: u32) {
        self.region.prefetch(self.bucket(hash));
    }

    /// Hint that the record at a spot will be read soon.
    #[inline(always)]
    pub(super) fn prefetch_record(&self, spot: R::Spot) {
        self.region.prefetch_record(spot);
    }

    /// The record count as last read.
    #[inline(always)]
    pub(super) fn records(&self) -> u64 {
        self.nrecords
    }

    /// Check that `steps` steps down a chain stay within the record count:
    /// a longer chain is corrupt.
    #[inline(always)]
    pub(super) fn check_steps(&mut self, steps: u64) -> Result<()> {
        if steps >= self.nrecords {
            self.refresh();
            if steps >= self.nrecords {
                return Err(cycle());
            }
        }
        Ok(())
    }

    /// The record at a place `place` accepted.
    ///
    /// # Safety
    ///
    /// The record lies within its chunk, as `place` checks, and is
    /// published, or the caller is its chunk's one writer.
    #[inline(always)]
    pub(super) unsafe fn view(&self, place: Place) -> View<'r> {
        // SAFETY: the caller's contract.
        unsafe { self.view_at(self.spot(place)) }
    }

    /// The record at a spot, as [`Self::view`] gives it.
    ///
    /// # Safety
    ///
    /// As [`Self::view`], for the place of the spot.
    #[inline(always)]
    pub(super) unsafe fn view_at(&self, spot: R::Spot) -> View<'r> {
        let (nkeys, payload_size) = (self.nkeys, self.payload_size);
        // SAFETY: the caller's contract: a located record lies within its
        // chunk and is never written again once published.
        let bytes = unsafe { self.region.record(spot, self.record_size) };
        // SAFETY: the header checked that the record size is the 16 bytes
        // of the record header, 8 per key and the payload, rounded up, so
        // the three parts lie within `bytes`; the record starts at a
        // multiple of 8 in a region aligned to 8, so the key slots, 16
        // bytes in, are aligned for `i64`, and any 8 bytes are an `i64`.
        unsafe {
            let base = bytes.as_ptr();
            View {
                header: &*base.cast::<[u8; RECORD_HEADER]>(),
                keys: core::slice::from_raw_parts(base.add(RECORD_HEADER).cast::<i64>(), nkeys),
                payload: core::slice::from_raw_parts(
                    base.add(RECORD_HEADER + nkeys * KEY_SLOT),
                    payload_size,
                ),
            }
        }
    }

    /// The byte offset of the bucket of a hash.
    #[inline]
    fn bucket(&self, hash: u32) -> usize {
        self.buckets_offset + (hash >> self.bucket_shift) as usize * 4
    }

    /// The first record of the chain of a hash's bucket, 0 for none.
    #[inline]
    pub(super) fn head(&self, hash: u32) -> u32 {
        // SAFETY: `hash >> bucket_shift` is below the bucket count, since
        // the shift leaves as many bits as the count's logarithm, and the
        // bucket array was checked to lie within the region.
        unsafe { self.region.load_u32_in(self.bucket(hash)) }
    }

    /// Walk the chain from `offset` to the first record with `hash` that
    /// `matches` and return its offset, 0 for none (a record never lies at
    /// 0, the header does); a chain longer than the record count is
    /// corrupt.
    #[inline(always)]
    pub(super) fn find(
        &mut self,
        mut offset: u32,
        hash: u32,
        mut matches: impl FnMut(&View<'r>) -> bool,
    ) -> Result<u32> {
        let mut steps = 0;
        while offset != 0 {
            if steps == self.nrecords {
                self.refresh();
                if steps >= self.nrecords {
                    return Err(cycle());
                }
            }
            steps += 1;
            let view = self.locate(offset)?;
            if view.hash() == hash && matches(&view) {
                return Ok(offset);
            }
            offset = view.next();
        }
        Ok(0)
    }

    /// The used mark of a chunk this operation writes, and how many more
    /// records fit it; a mark that is not a record boundary within the
    /// chunk is corrupt.
    #[inline]
    pub(super) fn room(&self, chunk: usize) -> Result<(usize, usize)> {
        if chunk >= self.region.chunks() {
            return Err(no_chunk(chunk));
        }
        let len = self.region.chunk_len(chunk);
        // SAFETY: the chunk exists, and the caller is its one writer.
        let used = unsafe { self.region.chunk_used(chunk) };
        let used = usize::try_from(used)
            .ok()
            .filter(|&used| {
                used >= CHUNK_HEADER
                    && used <= len
                    && (used - CHUNK_HEADER).is_multiple_of(self.record_size)
            })
            .ok_or_else(|| bad_used(chunk, used))?;
        Ok((used, (len - used) / self.record_size))
    }

    /// Store the used mark of a chunk this operation wrote.
    ///
    /// # Safety
    ///
    /// The caller is the chunk's one writer, and `used` is a record
    /// boundary within it.
    #[inline]
    pub(super) unsafe fn set_used(&self, chunk: usize, used: usize) {
        // SAFETY: the caller's contract.
        unsafe { self.region.set_chunk_used(chunk, used as u64) };
    }

    /// Write the record at a place of a chunk this operation appends to,
    /// from row `bit` of a word's keys; `payload` is the row's payload,
    /// `None` for zeros. `N` is the key count and
    /// `T` the words after the keys (payload and padding) when the caller
    /// knows them, 0 to take them from the layout.
    ///
    /// Everything past the header is written in 8-byte words: the key
    /// slots, then the payload and its padding up to the record size, so
    /// that a payload of a few bytes costs a store or two, not a call.
    ///
    /// # Safety
    ///
    /// The place lies past the chunk's used mark and within it, and this
    /// operation is the chunk's one writer; `keys` was made for this
    /// table's key count; `N` and `T` are 0 or this table's.
    #[inline(always)]
    pub(super) unsafe fn write<const N: usize, const T: usize>(
        &self,
        (chunk, byte): Place,
        hash: u32,
        keys: &WordKeys,
        bit: usize,
        payload: Option<&[u8]>,
    ) {
        let record_size = self.record_size;
        debug_assert!(N == 0 || N == self.nkeys);
        // SAFETY: the caller's contract: the record lies within the chunk,
        // past its used mark, and nothing else reads or writes it yet.
        let bytes = unsafe {
            self.region
                .record_mut(self.region.spot(chunk, byte), record_size)
        };
        // SAFETY: the caller's contract on `keys`, `N` and `T`.
        unsafe { fill::<N, T>(bytes, self.nkeys, hash, keys, bit, payload) };
    }
}

/// Write a record into `bytes`, a record's length, from row `bit` of a
/// word's keys: its header with no next record, the key slots, then the
/// payload (`None` for zeros) and its padding. `N` is the key count and
/// `T` the words after the keys when the caller knows them, 0 to take
/// them from `nkeys` and the length.
///
/// # Safety
///
/// `keys` was made for `nkeys` keys; `N` and `T` are 0 or the table's.
#[inline(always)]
pub(super) unsafe fn fill<const N: usize, const T: usize>(
    bytes: &mut [u8],
    nkeys: usize,
    hash: u32,
    keys: &WordKeys,
    bit: usize,
    payload: Option<&[u8]>,
) {
    {
        let record_size = bytes.len();
        let nkeys = if N > 0 { N } else { nkeys };
        debug_assert!(T == 0 || T == record_size / 8 - RECORD_HEADER / 8 - nkeys);
        let (words, _) = bytes.as_chunks_mut::<8>();
        let mut fields = [0; RECORD_HEADER];
        fields[HASH..HASH + 4].copy_from_slice(&hash.to_ne_bytes());
        fields[NULL_BITS..NULL_BITS + 4].copy_from_slice(&keys.null_bits(bit).to_ne_bytes());
        fields[LEN..LEN + 4].copy_from_slice(&((record_size / 8) as u32).to_ne_bytes());
        let (header, rest) = words.split_at_mut(RECORD_HEADER / 8);
        let (first, second) = fields.split_at(8);
        header[0] = first.try_into().unwrap();
        header[1] = second.try_into().unwrap();
        let (slots, tail) = rest.split_at_mut(nkeys);
        for (key, slot) in slots.iter_mut().enumerate() {
            // SAFETY: `key < nkeys`, the buffer's key count.
            *slot = unsafe { keys.key(key, bit) }.to_ne_bytes();
        }
        if T > 0
            && let Some(tail) = tail.first_chunk_mut::<T>()
        {
            match payload.map(<[u8]>::as_chunks::<8>) {
                None => {
                    *tail = [[0; 8]; T];
                    return;
                }
                Some((full, [])) => {
                    if let Some(full) = full.first_chunk::<T>() {
                        *tail = *full;
                        return;
                    }
                }
                Some(_) => {}
            }
        }
        if T > 0 {
            // A known shape whose payload did not fit it: a payload of a
            // partial last word. Kept out of the row loop.
            write_tail_cold(tail, payload);
        } else {
            write_tail(tail, payload);
        }
    }
}

impl<'r, R: Region> Access<'r, R> {
    /// The key count of a record.
    #[inline(always)]
    pub(super) fn nkeys(&self) -> usize {
        self.nkeys
    }

    /// Bytes of payload per record.
    #[inline(always)]
    pub(super) fn payload_size(&self) -> usize {
        self.payload_size
    }

    /// Publish a written record as the first of its bucket's chain.
    ///
    /// # Safety
    ///
    /// The record at `place`, whose reference is `offset`, lies within its
    /// chunk and is not published yet, and no one else publishes it.
    #[inline]
    pub(super) unsafe fn push(&self, offset: u32, place: Place, hash: u32) {
        // SAFETY: the caller's contract.
        unsafe { self.push_at(offset, self.spot(place), hash) };
    }

    /// As [`Self::push`], for the record at a spot the caller resolved;
    /// returns the head the record was put before, the rest of its chain.
    ///
    /// # Safety
    ///
    /// As [`Self::push`], `spot` being the record's.
    #[inline(always)]
    pub(super) unsafe fn push_at(&self, offset: u32, spot: R::Spot, hash: u32) -> u32 {
        let bucket = self.bucket(hash);
        let next = R::advance(spot, NEXT);
        // SAFETY: the bucket lies in the bucket array, as in `head`, and
        // the next field in the record, the caller's alone until published.
        unsafe {
            let mut head = self.region.load_u32_in(bucket);
            loop {
                self.region.store_next(next, head);
                match self.region.cas_u32_in(bucket, head, offset) {
                    Ok(_) => return head,
                    Err(found) => head = found,
                }
            }
        }
    }

    /// Publish a written record right after a published one with the same
    /// hash and keys, so that the records of a key stay next to each other
    /// in their chain.
    ///
    /// # Safety
    ///
    /// The caller has the table to itself; the record at `place`, whose
    /// reference is `offset`, lies within its chunk, and `after` is a
    /// record it located.
    #[inline]
    pub(super) unsafe fn link_after(&self, after: u32, (chunk, byte): Place, offset: u32) {
        let (after_chunk, after_byte) = placement(after);
        // SAFETY: both next fields lie in records within their chunks, and
        // nothing else reads or writes the table meanwhile.
        unsafe {
            let after = self.region.spot(after_chunk, after_byte + NEXT);
            let next = self.region.load_next(after);
            self.region
                .store_next(self.region.spot(chunk, byte + NEXT), next);
            self.region.store_next(after, offset);
        }
    }

    /// The reference of a place.
    #[inline(always)]
    pub(super) fn reference(&self, (chunk, byte): Place) -> u32 {
        reference(chunk, byte)
    }

    /// Count records published.
    #[inline]
    pub(super) fn count(&mut self, added: usize) {
        self.region.fetch_add_u64(NRECORDS, added as u64);
        self.nrecords += added as u64;
    }
}

/// Write a record's words after its keys: the payload, a partial last
/// word padded with zeros, then zeros up to the record's end.
#[inline(always)]
fn write_tail(tail: &mut [[u8; 8]], payload: Option<&[u8]>) {
    let mut filled = 0;
    if let Some(payload) = payload {
        let (full, rem) = payload.as_chunks::<8>();
        let (head, rest) = tail.split_at_mut(full.len());
        copy_words(head, full);
        filled = full.len();
        if let Some((slot, _)) = rest.split_first_mut()
            && !rem.is_empty()
        {
            let mut last = [0; 8];
            for (to, from) in last.iter_mut().zip(rem) {
                *to = *from;
            }
            *slot = last;
            filled += 1;
        }
    }
    zero_words(&mut tail[filled..]);
}

/// [`write_tail`] out of line, for the shapes whose common case is
/// written inline: its calls would make the row loop save its registers.
#[cold]
#[inline(never)]
fn write_tail_cold(tail: &mut [[u8; 8]], payload: Option<&[u8]>) {
    write_tail(tail, payload);
}

/// Words a record copies with plain stores before a call to `memcpy` or
/// `memset` pays for itself: payloads of joins on a few keys and
/// aggregate states fit, wider build rows take the call.
const INLINE_WORDS: usize = 4;

/// Copy `src` into `dst` of the same length. Written out word by word up
/// to [`INLINE_WORDS`], since a loop would be turned into a call.
#[inline(always)]
fn copy_words(dst: &mut [[u8; 8]], src: &[[u8; 8]]) {
    let n = dst.len().min(src.len());
    if n > INLINE_WORDS {
        dst[..n].copy_from_slice(&src[..n]);
        return;
    }
    if n > 0 {
        dst[0] = src[0];
    }
    if n > 1 {
        dst[1] = src[1];
    }
    if n > 2 {
        dst[2] = src[2];
    }
    if n > 3 {
        dst[3] = src[3];
    }
}

/// Zero `dst`, word by word up to [`INLINE_WORDS`], as [`copy_words`].
#[inline(always)]
fn zero_words(dst: &mut [[u8; 8]]) {
    let n = dst.len();
    if n > INLINE_WORDS {
        dst.fill([0; 8]);
        return;
    }
    if n > 0 {
        dst[0] = [0; 8];
    }
    if n > 1 {
        dst[1] = [0; 8];
    }
    if n > 2 {
        dst[2] = [0; 8];
    }
    if n > 3 {
        dst[3] = [0; 8];
    }
}

/// The error of a record offset outside the records, kept out of the
/// loops that walk chains.
#[cold]
#[inline(never)]
fn outside(offset: u32) -> anyhow::Error {
    anyhow::anyhow!("table record offset {offset} lies outside the records")
}

/// The error of a chunk number past the chunks.
#[cold]
#[inline(never)]
pub(super) fn no_chunk(chunk: usize) -> anyhow::Error {
    anyhow::anyhow!("table chunk {chunk} does not exist")
}

/// The error of a used mark that is not a record boundary of its chunk.
#[cold]
#[inline(never)]
fn bad_used(chunk: usize, used: u64) -> anyhow::Error {
    anyhow::anyhow!("table chunk {chunk} has a used mark of {used} bytes that ends no record")
}

/// The error of an offset that does not start a record.
#[cold]
#[inline(never)]
fn misplaced(offset: u32) -> anyhow::Error {
    anyhow::anyhow!("table record offset {offset} does not start a record")
}

/// The error of a chain longer than the record count.
#[cold]
#[inline(never)]
fn cycle() -> anyhow::Error {
    anyhow::anyhow!("table chain is longer than its record count")
}
