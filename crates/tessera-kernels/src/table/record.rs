//! Records: how one lies in the region and how it is found, written and
//! published.
//!
//! A record starts with 16 bytes: its hash, the offset of the next record
//! of its bucket, the bits of its keys that are NULL and its length in
//! 8-byte units; then one 8-byte slot per key and the payload, rounded up
//! to 8. Offsets are in 8-byte units; 0 is none. A record is written in
//! reserved, unpublished bytes and published by a compare-and-swap of its
//! bucket's head, after which it never changes.

use anyhow::{Result, ensure};

use super::header::{CHUNK_USED, HEADER_SIZE, KEY_SLOT, Layout, NRECORDS, RECORD_HEADER};
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

/// The bytes of a published record.
pub(super) struct View<'r> {
    bytes: &'r [u8],
    nkeys: usize,
    payload_size: usize,
}

impl<'r> View<'r> {
    fn field(&self, at: usize) -> u32 {
        u32::from_ne_bytes(self.bytes[at..at + 4].try_into().unwrap())
    }

    pub(super) fn hash(&self) -> u32 {
        self.field(HASH)
    }

    pub(super) fn null_bits(&self) -> u32 {
        self.field(NULL_BITS)
    }

    /// The length the record claims, in bytes.
    fn len(&self) -> usize {
        self.field(LEN) as usize * 8
    }

    pub(super) fn keys(&self) -> &'r [i64] {
        let bytes: &'r [u8] = self.bytes;
        let slots = &bytes[RECORD_HEADER..RECORD_HEADER + self.nkeys * KEY_SLOT];
        // SAFETY: the slots start 16 bytes into a record that lies at a
        // multiple of 8 in a region aligned to 8, so they are aligned to 8;
        // any 8 bytes are a valid `i64`; the slice borrows for `'r` like
        // the bytes it is cut from.
        unsafe { core::slice::from_raw_parts(slots.as_ptr().cast::<i64>(), self.nkeys) }
    }

    pub(super) fn payload(&self) -> &'r [u8] {
        let bytes: &'r [u8] = self.bytes;
        let start = RECORD_HEADER + self.nkeys * KEY_SLOT;
        &bytes[start..start + self.payload_size]
    }

    pub(super) fn record(&self) -> Record<'r> {
        Record {
            hash: self.hash(),
            null_bits: self.null_bits(),
            keys: self.keys(),
            payload: self.payload(),
        }
    }
}

/// Record-level access during one operation, with the counters as read at
/// its start.
pub(super) struct Access<'r, R> {
    region: &'r R,
    layout: &'r Layout,
    used: usize,
    nrecords: u64,
}

impl<'r, R: Region> Access<'r, R> {
    pub(super) fn new(region: &'r R, layout: &'r Layout) -> Self {
        let mut access = Self {
            region,
            layout,
            used: 0,
            nrecords: 0,
        };
        access.refresh();
        access
    }

    fn refresh(&mut self) {
        self.used = self.region.load_u64(CHUNK_USED) as usize;
        self.nrecords = self.region.load_u64(NRECORDS);
    }

    fn in_records(&self, byte: usize) -> bool {
        byte >= HEADER_SIZE && byte + self.layout.record_size <= self.used
    }

    /// The record at `offset`, which must lie among the records published
    /// so far and claim the table's record length.
    pub(super) fn locate(&mut self, offset: u32) -> Result<View<'r>> {
        let byte = offset as usize * 8;
        if !self.in_records(byte) {
            // A record published since the counters were read lies past
            // them: read again before calling the region corrupt.
            self.refresh();
            ensure!(
                self.in_records(byte),
                "table record offset {offset} lies outside the records"
            );
        }
        let view = self.view(byte);
        ensure!(
            view.len() == self.layout.record_size,
            "table record offset {offset} does not start a record"
        );
        Ok(view)
    }

    /// The published record at a byte offset that `locate` accepted.
    fn view(&self, byte: usize) -> View<'r> {
        // SAFETY: a located record is published and never written again,
        // and its bytes lie among the records, below the buckets.
        let bytes = unsafe { self.region.bytes(byte, self.layout.record_size) };
        View {
            bytes,
            nkeys: self.layout.nkeys,
            payload_size: self.layout.payload_size,
        }
    }

    /// The byte offset of the bucket of a hash.
    fn bucket(&self, hash: u32) -> usize {
        self.layout.buckets_offset + (hash >> self.layout.bucket_shift) as usize * 4
    }

    /// Reserve room for up to `wanted` records: the byte offset of the
    /// first and how many fit, or `None` when none does.
    pub(super) fn reserve(&mut self, wanted: usize) -> Option<(usize, usize)> {
        let record_size = self.layout.record_size;
        loop {
            let room = self.layout.buckets_offset.saturating_sub(self.used) / record_size;
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
                Err(found) => self.used = found as usize,
            }
        }
    }

    /// Write a reserved record; `payload` is `None` for zeros.
    pub(super) fn write(
        &self,
        byte: usize,
        hash: u32,
        null_bits: u32,
        keys: impl Iterator<Item = i64>,
        payload: Option<&[u8]>,
    ) {
        let record_size = self.layout.record_size;
        // SAFETY: the record was reserved by this operation and is not
        // published yet, so nothing else reads or writes its bytes.
        let bytes = unsafe { self.region.bytes_mut(byte, record_size) };
        bytes[HASH..HASH + 4].copy_from_slice(&hash.to_ne_bytes());
        bytes[NEXT..NEXT + 4].copy_from_slice(&0_u32.to_ne_bytes());
        bytes[NULL_BITS..NULL_BITS + 4].copy_from_slice(&null_bits.to_ne_bytes());
        bytes[LEN..LEN + 4].copy_from_slice(&((record_size / 8) as u32).to_ne_bytes());
        let mut at = RECORD_HEADER;
        for key in keys {
            bytes[at..at + KEY_SLOT].copy_from_slice(&key.to_ne_bytes());
            at += KEY_SLOT;
        }
        let (area, padding) = bytes[at..].split_at_mut(self.layout.payload_size);
        match payload {
            Some(payload) => area.copy_from_slice(payload),
            None => area.fill(0),
        }
        padding.fill(0);
    }

    /// Publish a written record as the newest of its bucket.
    pub(super) fn push(&self, offset: u32, byte: usize, hash: u32) {
        let bucket = self.bucket(hash);
        let mut head = self.region.load_u32(bucket);
        loop {
            self.region.store_u32(byte + NEXT, head);
            match self.region.cas_u32(bucket, head, offset) {
                Ok(_) => return,
                Err(found) => head = found,
            }
        }
    }

    /// Count records published.
    pub(super) fn count(&mut self, added: usize) {
        self.region.fetch_add_u64(NRECORDS, added as u64);
        self.nrecords += added as u64;
    }
}
