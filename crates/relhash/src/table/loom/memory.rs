//! The memory of a table under loom: a second [`Region`] over loom's
//! atomics, which the table's own generic code runs over.

use std::cell::UnsafeCell as StdUnsafeCell;
use std::sync::atomic::{AtomicU8, AtomicU64 as PlainU64, Ordering as Plain};

use crate::error::Result;
use ::loom::cell::UnsafeCell;
use ::loom::sync::atomic::{AtomicU32, AtomicU64};
use core::sync::atomic::Ordering;

use super::model::CONFIG;
use crate::table::header::{CHUNK_HEADER, HEADER_SIZE, NRECORDS, chunk_layout};
use crate::table::index_size;
use crate::table::region::{Region, order};

/// How the model publishes records: the orderings of the bucket heads,
/// the region's or relaxed ones, and whether the count of records goes up
/// before a record is published or only after it. The relaxed orderings
/// and the late count are the mistakes the model must notice.
#[derive(Clone, Copy)]
pub(super) struct Heads {
    pub(super) load: Ordering,
    pub(super) cas: Ordering,
    pub(super) cas_failed: Ordering,
    pub(super) late_count: bool,
}

pub(super) const HEADS: Heads = Heads {
    load: order::LOAD,
    cas: order::CAS,
    cas_failed: order::CAS_FAILED,
    late_count: false,
};

/// A chunk of the model: its bytes, a loom cell for its used mark and one
/// per record it may hold.
struct LoomChunk {
    bytes: StdUnsafeCell<Box<[u64]>>,
    used: UnsafeCell<()>,
    records: Vec<UnsafeCell<()>>,
}

/// An index of loom atomics and chunks of tracked bytes.
pub(super) struct LoomRegion {
    len: usize,
    record_size: usize,
    heads: Heads,
    /// The header's 8-byte and 4-byte fields; `widths` records which of
    /// the two a 4-byte word was used as, which must never change.
    header64: Vec<AtomicU64>,
    header32: Vec<AtomicU32>,
    widths: Vec<AtomicU8>,
    buckets: Vec<AtomicU32>,
    chunks: Vec<LoomChunk>,
    chunk_len: usize,
    /// Under [`LATE_COUNT`], what a link added to the count of records and
    /// the model holds back until the link's next record is published;
    /// one participant links.
    held: PlainU64,
}

// SAFETY: loom runs one thread at a time, and every access to the bytes of
// a chunk is announced to a loom cell first, which reports any two accesses
// that are not ordered; the slices lent out cover the records the table's
// contract gives the caller.
unsafe impl Sync for LoomRegion {}
// SAFETY: as for `Sync`.
unsafe impl Send for LoomRegion {}

impl LoomRegion {
    /// An index for `capacity` records of `CONFIG` and `nchunks` empty
    /// chunks of `records` records each.
    pub(super) fn new(capacity: u64, nchunks: usize, records: usize, heads: Heads) -> Result<Self> {
        let len = index_size(&CONFIG, capacity)?;
        let record_size = chunk_layout(&CONFIG)?.record_size;
        let chunk_len = CHUNK_HEADER + records * record_size;
        let chunks = (0..nchunks)
            .map(|_| {
                let mut words = vec![0; chunk_len / 8].into_boxed_slice();
                words[0] = CHUNK_HEADER as u64;
                LoomChunk {
                    bytes: StdUnsafeCell::new(words),
                    used: UnsafeCell::new(()),
                    records: (0..records).map(|_| UnsafeCell::new(())).collect(),
                }
            })
            .collect();
        Ok(Self {
            len,
            record_size,
            heads,
            header64: (0..HEADER_SIZE / 8).map(|_| AtomicU64::new(0)).collect(),
            header32: (0..HEADER_SIZE / 4).map(|_| AtomicU32::new(0)).collect(),
            widths: (0..HEADER_SIZE / 4).map(|_| AtomicU8::new(0)).collect(),
            buckets: (0..(len - HEADER_SIZE) / 4)
                .map(|_| AtomicU32::new(0))
                .collect(),
            chunks,
            chunk_len,
            held: PlainU64::new(0),
        })
    }

    /// Note that the header word at `offset` is used `width` bytes wide.
    fn width(&self, offset: usize, width: u8) {
        for word in offset / 4..(offset + usize::from(width)) / 4 {
            let seen = self.widths[word].swap(width, Plain::Relaxed);
            assert!(
                seen == 0 || seen == width,
                "header byte {offset} read as {width} bytes and as {seen}"
            );
        }
    }

    fn header64(&self, offset: usize) -> &AtomicU64 {
        assert!(offset + 8 <= HEADER_SIZE && offset.is_multiple_of(8));
        self.width(offset, 8);
        &self.header64[offset / 8]
    }

    fn header32(&self, offset: usize) -> &AtomicU32 {
        assert!(offset + 4 <= HEADER_SIZE && offset.is_multiple_of(4));
        self.width(offset, 4);
        &self.header32[offset / 4]
    }

    fn bucket(&self, offset: usize) -> &AtomicU32 {
        assert!(offset >= HEADER_SIZE, "a bucket lies past the header");
        &self.buckets[(offset - HEADER_SIZE) / 4]
    }

    /// The loom cells of the records `len` bytes at `byte` of a chunk touch.
    fn cells(&self, chunk: usize, byte: usize, len: usize) -> &[UnsafeCell<()>] {
        assert!(byte >= CHUNK_HEADER && byte + len <= self.chunk_len);
        let first = (byte - CHUNK_HEADER) / self.record_size;
        let last = (byte + len - 1 - CHUNK_HEADER) / self.record_size;
        &self.chunks[chunk].records[first..=last]
    }

    fn address(&self, chunk: usize, byte: usize) -> *mut u8 {
        // SAFETY: the byte lies within the chunk's words, as `cells` or the
        // used mark's word checks.
        unsafe {
            (*self.chunks[chunk].bytes.get())
                .as_mut_ptr()
                .cast::<u8>()
                .add(byte)
        }
    }

    fn read(&self, chunk: usize, byte: usize, len: usize) -> *const u8 {
        for cell in self.cells(chunk, byte, len) {
            cell.with(|_| ());
        }
        self.address(chunk, byte)
    }

    fn write(&self, chunk: usize, byte: usize, len: usize) -> *mut u8 {
        for cell in self.cells(chunk, byte, len) {
            cell.with_mut(|_| ());
        }
        self.address(chunk, byte)
    }
}

impl Region for LoomRegion {
    fn len(&self) -> usize {
        self.len
    }

    fn store_u32(&self, offset: usize, value: u32) {
        self.header32(offset).store(value, order::STORE);
    }

    fn load_u32_relaxed(&self, offset: usize) -> u32 {
        self.header32(offset).load(order::RELAXED)
    }

    fn load_u64_relaxed(&self, offset: usize) -> u64 {
        self.header64(offset).load(order::RELAXED)
    }

    fn load_u64(&self, offset: usize) -> u64 {
        self.header64(offset).load(order::LOAD)
    }

    fn store_u64(&self, offset: usize, value: u64) {
        self.header64(offset).store(value, order::STORE);
    }

    fn fetch_add_u64(&self, offset: usize, delta: u64) -> u64 {
        if self.heads.late_count && offset == NRECORDS {
            self.held.fetch_add(delta, Plain::Relaxed);
            return self.header64(offset).load(order::LOAD);
        }
        self.header64(offset).fetch_add(delta, order::ADD)
    }

    /// The model's buckets are created zero and a model index is made
    /// once, so there is nothing to clear; a thousand stores would each be
    /// a branch of the model.
    unsafe fn zero_u32(&self, offset: usize, count: usize) {
        assert_eq!(offset, HEADER_SIZE, "only the buckets are cleared");
        assert_eq!(count, self.buckets.len(), "the buckets are cleared whole");
    }

    unsafe fn load_u32_in(&self, offset: usize) -> u32 {
        self.bucket(offset).load(self.heads.load)
    }

    unsafe fn cas_u32_in(&self, offset: usize, current: u32, new: u32) -> Result<u32, u32> {
        let swapped = self.bucket(offset).compare_exchange(
            current,
            new,
            self.heads.cas,
            self.heads.cas_failed,
        );
        if self.heads.late_count && swapped.is_ok() {
            let held = self.held.swap(0, Plain::Relaxed);
            if held > 0 {
                self.header64(NRECORDS).fetch_add(held, order::ADD);
            }
        }
        swapped
    }

    fn prefetch(&self, _offset: usize) {}

    fn chunks(&self) -> usize {
        self.chunks.len()
    }

    fn chunk_len(&self, chunk: usize) -> usize {
        assert!(chunk < self.chunks.len());
        self.chunk_len
    }

    /// The model's chunks are its own vectors of words.
    fn chunk_fits(&self, chunk: usize) -> bool {
        assert!(chunk < self.chunks.len());
        true
    }

    unsafe fn chunk_used(&self, chunk: usize) -> u64 {
        self.chunks[chunk].used.with(|_| ());
        // SAFETY: the used mark is the chunk's first word.
        unsafe { self.address(chunk, 0).cast::<u64>().read() }
    }

    unsafe fn set_chunk_used(&self, chunk: usize, used: u64) {
        self.chunks[chunk].used.with_mut(|_| ());
        // SAFETY: as for `chunk_used`.
        unsafe { self.address(chunk, 0).cast::<u64>().write(used) }
    }

    type Spot = (usize, usize);

    unsafe fn spot(&self, chunk: usize, byte: usize) -> (usize, usize) {
        assert!(chunk < self.chunks.len() && byte <= self.chunk_len);
        (chunk, byte)
    }

    fn advance((chunk, byte): (usize, usize), bytes: usize) -> (usize, usize) {
        (chunk, byte + bytes)
    }

    unsafe fn record(&self, (chunk, byte): (usize, usize), len: usize) -> &[u8] {
        // SAFETY: within the chunk, the cells checked.
        unsafe { core::slice::from_raw_parts(self.read(chunk, byte, len), len) }
    }

    unsafe fn record_mut(&self, (chunk, byte): (usize, usize), len: usize) -> &mut [u8] {
        // SAFETY: as in `record`.
        unsafe { core::slice::from_raw_parts_mut(self.write(chunk, byte, len), len) }
    }

    unsafe fn load_next(&self, (chunk, byte): (usize, usize)) -> u32 {
        // SAFETY: four bytes of a record, aligned to 4.
        unsafe { self.read(chunk, byte, 4).cast::<u32>().read() }
    }

    unsafe fn store_next(&self, (chunk, byte): (usize, usize), value: u32) {
        // SAFETY: as in `load_next`.
        unsafe { self.write(chunk, byte, 4).cast::<u32>().write(value) }
    }

    fn prefetch_record(&self, _spot: (usize, usize)) {}
}
