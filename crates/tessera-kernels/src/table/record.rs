//! Records: how one lies in the region and how it is found, written and
//! published.
//!
//! A record starts with 16 bytes: its hash, the offset of the next record
//! of its bucket, the bits of its keys that are NULL and its length in
//! 8-byte units; then one 8-byte slot per key and the payload, rounded up
//! to 8. Offsets are in 8-byte units; 0 is none. A record is written in
//! reserved, unpublished bytes and published by a compare-and-swap of its
//! bucket's head, after which it never changes.

use anyhow::Result;

use super::header::{CHUNK_USED, HEADER_SIZE, KEY_SLOT, Layout, NRECORDS, RECORD_HEADER};
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
    used: usize,
    nrecords: u64,
}

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
            used: 0,
            nrecords: 0,
        };
        access.refresh();
        access
    }

    /// Bytes of a record.
    #[inline(always)]
    pub(super) fn record_size(&self) -> usize {
        self.record_size
    }

    /// Read the counters again. The end of the record area is clamped to
    /// the buckets' offset, which attachment checked against the region:
    /// whatever the shared bytes say later, a record found below it lies
    /// within the region, which the unchecked accesses rely on.
    #[inline]
    fn refresh(&mut self) {
        let used = self.region.load_u64(CHUNK_USED);
        self.used =
            usize::try_from(used).map_or(self.buckets_offset, |used| used.min(self.buckets_offset));
        self.nrecords = self.region.load_u64(NRECORDS);
    }

    #[inline]
    fn in_records(&self, byte: usize) -> bool {
        byte >= HEADER_SIZE && byte + self.record_size <= self.used
    }

    /// The record at `offset`, which must lie among the records published
    /// so far and claim the table's record length.
    #[inline(always)]
    pub(super) fn locate(&mut self, offset: u32) -> Result<View<'r>> {
        let byte = offset as usize * 8;
        if !self.in_records(byte) {
            // A record published since the counters were read lies past
            // them: read again before calling the region corrupt.
            self.refresh();
            if !self.in_records(byte) {
                return Err(outside(offset));
            }
        }
        let view = self.view(byte);
        if view.len() != self.record_size {
            return Err(misplaced(offset));
        }
        Ok(view)
    }

    /// The published record at a byte offset that `locate` accepted.
    #[inline(always)]
    fn view(&self, byte: usize) -> View<'r> {
        let (nkeys, payload_size) = (self.nkeys, self.payload_size);
        // SAFETY: a located record is published and never written again,
        // and its bytes lie among the records, below the used mark, which
        // is at most the buckets' offset and so within the region.
        let bytes = unsafe { self.region.bytes_in(byte, self.record_size) };
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

    /// The newest record of the bucket of a hash, 0 for none.
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

    /// Reserve room for up to `wanted` records: the byte offset of the
    /// first and how many fit, or `None` when none does.
    #[inline]
    pub(super) fn reserve(&mut self, wanted: usize) -> Option<(usize, usize)> {
        let record_size = self.record_size;
        loop {
            let room = self.buckets_offset.saturating_sub(self.used) / record_size;
            let count = wanted.min(room);
            if count == 0 {
                return None;
            }
            let end = self.used + count * record_size;
            match self
                .region
                .cas_u64(CHUNK_USED, self.used as u64, end as u64)
            {
                Ok(_) => {
                    let start = self.used;
                    self.used = end;
                    return Some((start, count));
                }
                // Clamped as in `refresh`: a used mark past the buckets
                // leaves no room.
                Err(found) => self.used = (found as usize).min(self.buckets_offset),
            }
        }
    }

    /// Write a reserved record from row `bit` of a word's keys; `payload`
    /// is the row's payload, `None` for zeros. `N` is the key count and
    /// `T` the words after the keys (payload and padding) when the caller
    /// knows them, 0 to take them from the layout.
    ///
    /// Everything past the header is written in 8-byte words: the key
    /// slots, then the payload and its padding up to the record size, so
    /// that a payload of a few bytes costs a store or two, not a call.
    ///
    /// # Safety
    ///
    /// `byte` starts a record this operation reserved; `keys` was made for
    /// this table's key count; `N` and `T` are 0 or this table's.
    #[inline(always)]
    pub(super) unsafe fn write<const N: usize, const T: usize>(
        &self,
        byte: usize,
        hash: u32,
        keys: &WordKeys,
        bit: usize,
        payload: Option<&[u8]>,
    ) {
        let record_size = self.record_size;
        let nkeys = if N > 0 { N } else { self.nkeys };
        debug_assert!(nkeys == self.nkeys);
        debug_assert!(T == 0 || T == record_size / 8 - RECORD_HEADER / 8 - nkeys);
        // SAFETY: the record was reserved by this operation and is not
        // published yet, so nothing else reads or writes its bytes; a
        // reservation ends at most at the buckets' offset, in the region.
        let bytes = unsafe { self.region.bytes_mut_in(byte, record_size) };
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

    /// Publish a written record as the newest of its bucket.
    #[inline]
    pub(super) fn push(&self, offset: u32, byte: usize, hash: u32) {
        let bucket = self.bucket(hash);
        // SAFETY: the bucket lies in the bucket array, as in `head`, and
        // the next field in the record this operation reserved.
        unsafe {
            let mut head = self.region.load_u32_in(bucket);
            loop {
                self.region.store_u32_in(byte + NEXT, head);
                match self.region.cas_u32_in(bucket, head, offset) {
                    Ok(_) => return,
                    Err(found) => head = found,
                }
            }
        }
    }

    /// Count records published.
    #[inline]
    pub(super) fn count(&mut self, added: usize) {
        self.region.fetch_add_u64(NRECORDS, added as u64);
        self.nrecords += added as u64;
    }
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
