//! The memory a table lives in: an index and chunks of records.
//!
//! A table never holds a process address. Its index is one block, handed
//! to every operation as a base pointer and a length: a header, then the
//! bucket array, addressed by offset from the start. Its records lie in
//! chunks, blocks of their own that never move, which the caller hands
//! over as arrays of their bases and lengths in this process; a record is
//! addressed by the number of its chunk and its offset inside, so the same
//! bytes mean the same to every process that maps them, wherever it maps
//! them. Counters and bucket heads that several participants may change at
//! once are read and written only through the atomic operations here;
//! record bytes, including a record's next field and a chunk's used mark,
//! are plain memory, written by one participant before the records are
//! published and read by the others after. [`RawRegion`] is the one
//! implementation, over raw pointers; the trait is the seam for a model
//! region under loom.
//!
//! The orderings live in [`order`], which the loom model in `loom.rs`
//! shares, so that the model checks the orderings this code runs with.
//!
//! Offsets are validated by the table before any call, so a violation here
//! is a bug: the methods assert it instead of returning an error. Every
//! method is marked for inlining: the table's loops are generic and
//! instantiated in the crate that calls them, where a plain method of this
//! crate would stay a call per row.

use core::sync::atomic::{AtomicU32, AtomicU64};

/// The orderings of the region's atomic operations, by method.
pub(super) mod order {
    use core::sync::atomic::Ordering;

    /// Fields no participant changes while the table is shared.
    pub(in super::super) const RELAXED: Ordering = Ordering::Relaxed;
    /// Reads of counters and bucket heads.
    pub(in super::super) const LOAD: Ordering = Ordering::Acquire;
    /// Writes of header fields.
    pub(in super::super) const STORE: Ordering = Ordering::Release;
    /// A compare-and-swap of a bucket head.
    pub(in super::super) const CAS: Ordering = Ordering::AcqRel;
    /// A compare-and-swap that failed and returns the value found.
    pub(in super::super) const CAS_FAILED: Ordering = Ordering::Acquire;
    /// An addition to the record count.
    pub(in super::super) const ADD: Ordering = Ordering::AcqRel;
}

/// The index a table reads and writes by offset, and its chunks of records.
pub(super) trait Region {
    /// The number of addressable bytes of the index.
    fn len(&self) -> usize;

    /// Write a 32-bit word of the index with release ordering.
    fn store_u32(&self, offset: usize, value: u32);

    /// Read a 32-bit word of the index without ordering: for fields no
    /// participant changes while the table is shared, written before it was.
    fn load_u32_relaxed(&self, offset: usize) -> u32;

    /// Read a 64-bit word without ordering, as [`Self::load_u32_relaxed`].
    fn load_u64_relaxed(&self, offset: usize) -> u64;

    /// Read a 64-bit word of the index with acquire ordering.
    fn load_u64(&self, offset: usize) -> u64;

    /// Write a 64-bit word of the index with release ordering.
    fn store_u64(&self, offset: usize, value: u64);

    /// Add to a 64-bit word of the index (acquire-release) and return its
    /// previous value.
    fn fetch_add_u64(&self, offset: usize, delta: u64) -> u64;

    /// Clear `count` 32-bit words of the index from `offset`.
    ///
    /// # Safety
    ///
    /// Nothing else reads or writes these words meanwhile: the caller has
    /// exclusive use of the index.
    unsafe fn zero_u32(&self, offset: usize, count: usize);

    /// Read a bucket head with acquire ordering, without the bounds check.
    ///
    /// # Safety
    ///
    /// `offset + 4` is within [`Self::len`], as the validated layout proves
    /// for a bucket.
    unsafe fn load_u32_in(&self, offset: usize) -> u32;

    /// Replace a bucket head if it still holds `current`, without the
    /// bounds check: `Ok` with the value replaced, `Err` with the value
    /// found (acquire-release).
    ///
    /// # Safety
    ///
    /// As [`Self::load_u32_in`].
    unsafe fn cas_u32_in(&self, offset: usize, current: u32, new: u32) -> Result<u32, u32>;

    /// Hint that the cache line at `offset` of the index will be read soon.
    /// Any offset is allowed: a hint neither faults nor changes anything.
    fn prefetch(&self, offset: usize);

    /// The number of chunks.
    fn chunks(&self) -> usize;

    /// The length of chunk `chunk` in bytes; `chunk` is below
    /// [`Self::chunks`].
    fn chunk_len(&self, chunk: usize) -> usize;

    /// The used mark of a chunk: the bytes its records take, its 8-byte
    /// header included, as its one writer last stored it.
    ///
    /// # Safety
    ///
    /// `chunk` is below [`Self::chunks`], and no other participant writes
    /// the mark meanwhile.
    unsafe fn chunk_used(&self, chunk: usize) -> u64;

    /// Store the used mark of a chunk.
    ///
    /// # Safety
    ///
    /// As [`Self::chunk_used`], and the caller is the chunk's one writer.
    unsafe fn set_chunk_used(&self, chunk: usize, used: u64);

    /// Borrow `len` bytes at `byte` of a chunk for reading.
    ///
    /// # Safety
    ///
    /// `byte + len` is within the chunk, and nothing writes these bytes
    /// while the slice lives: they hold a published record, or the caller
    /// is the chunk's one writer.
    unsafe fn record(&self, chunk: usize, byte: usize, len: usize) -> &[u8];

    /// Borrow `len` bytes at `byte` of a chunk for writing.
    ///
    /// # Safety
    ///
    /// `byte + len` is within the chunk, and nothing else reads or writes
    /// these bytes while the slice lives: the record is not published yet,
    /// or the caller has exclusive use of the table.
    #[allow(clippy::mut_from_ref)]
    unsafe fn record_mut(&self, chunk: usize, byte: usize, len: usize) -> &mut [u8];

    /// Read the 32-bit word at `byte` of a chunk: a record's next field.
    ///
    /// # Safety
    ///
    /// As [`Self::record`] for 4 bytes.
    unsafe fn load_next(&self, chunk: usize, byte: usize) -> u32;

    /// Write the 32-bit word at `byte` of a chunk: a record's next field,
    /// before the record is published or under the table's one writer.
    ///
    /// # Safety
    ///
    /// As [`Self::record_mut`] for 4 bytes.
    unsafe fn store_next(&self, chunk: usize, byte: usize, value: u32);

    /// Hint that the cache line at `byte` of a chunk will be read soon;
    /// any values are allowed.
    fn prefetch_record(&self, chunk: usize, byte: usize);
}

/// An index and its chunks over the caller's memory.
#[derive(Debug)]
pub(super) struct RawRegion {
    base: *mut u8,
    len: usize,
    chunk_bases: *const *mut u8,
    chunk_lens: *const usize,
    nchunks: usize,
}

impl RawRegion {
    /// Address the index of `len` bytes at `base` and the `nchunks` chunks
    /// whose bases and lengths `chunk_bases` and `chunk_lens` hold.
    ///
    /// # Safety
    ///
    /// `base` is aligned to 8 and valid for reads and writes of `len` bytes
    /// for as long as the region is used; `chunk_bases` and `chunk_lens`
    /// point to `nchunks` entries each (or may dangle when it is 0) that
    /// stay unchanged, and every chunk base is aligned to 8 and valid for
    /// reads and writes of its length, a multiple of 8 of at least 8 bytes.
    /// During that time the bytes are accessed only through tables over
    /// them, in one process or in several, each with a mapping of its own.
    #[inline]
    pub(super) unsafe fn new(
        base: *mut u8,
        len: usize,
        chunk_bases: *const *mut u8,
        chunk_lens: *const usize,
        nchunks: usize,
    ) -> Self {
        debug_assert!(base.addr().is_multiple_of(8));
        Self {
            base,
            len,
            chunk_bases,
            chunk_lens,
            nchunks,
        }
    }

    /// The same chunks with another index, of `len` bytes at `base`.
    ///
    /// # Safety
    ///
    /// As [`Self::new`] for the index.
    #[inline]
    pub(super) unsafe fn with_index(&self, base: *mut u8, len: usize) -> Self {
        debug_assert!(base.addr().is_multiple_of(8));
        Self {
            base,
            len,
            chunk_bases: self.chunk_bases,
            chunk_lens: self.chunk_lens,
            nchunks: self.nchunks,
        }
    }

    /// The address of `size` bytes at `offset` of the index, both checked.
    #[inline]
    fn at(&self, offset: usize, size: usize) -> *mut u8 {
        let end = offset.checked_add(size).expect("region offset overflows");
        assert!(end <= self.len, "region access past its end");
        // SAFETY: `offset` is within the `len` bytes the constructor promised.
        unsafe { self.base.add(offset) }
    }

    /// The address at `offset` of the index, which the caller proved in
    /// bounds.
    ///
    /// # Safety
    ///
    /// `offset + size` is within the `len` bytes of the index.
    #[inline(always)]
    unsafe fn at_in(&self, offset: usize, size: usize) -> *mut u8 {
        debug_assert!(offset.checked_add(size).is_some_and(|end| end <= self.len));
        // SAFETY: the caller's contract.
        unsafe { self.base.add(offset) }
    }

    /// The address at `byte` of a chunk, which the caller proved in bounds.
    ///
    /// # Safety
    ///
    /// `chunk` is below the chunk count and `byte + size` within its length.
    #[inline(always)]
    unsafe fn chunk_at(&self, chunk: usize, byte: usize, size: usize) -> *mut u8 {
        debug_assert!(chunk < self.nchunks);
        debug_assert!(byte + size <= self.chunk_len(chunk));
        // SAFETY: the caller's contract, and the constructor's for the arrays.
        unsafe { (*self.chunk_bases.add(chunk)).add(byte) }
    }

    /// The 32-bit atomic at `offset` of the index, a multiple of 4.
    #[inline]
    fn atomic_u32(&self, offset: usize) -> &AtomicU32 {
        let address = self.at(offset, 4);
        debug_assert!(address.addr().is_multiple_of(4));
        // SAFETY: the address is in bounds and aligned to 4 (the base is
        // aligned to 8 and offsets of 32-bit words are multiples of 4); the
        // words reached this way are accessed only atomically by every
        // table over the index, as the constructor's contract requires.
        unsafe { AtomicU32::from_ptr(address.cast()) }
    }

    /// The 64-bit atomic at `offset` of the index, a multiple of 8.
    #[inline]
    fn atomic_u64(&self, offset: usize) -> &AtomicU64 {
        let address = self.at(offset, 8);
        debug_assert!(address.addr().is_multiple_of(8));
        // SAFETY: as for the 32-bit atomics, with alignment 8.
        unsafe { AtomicU64::from_ptr(address.cast()) }
    }
}

impl Region for RawRegion {
    #[inline]
    fn len(&self) -> usize {
        self.len
    }

    #[inline]
    fn store_u32(&self, offset: usize, value: u32) {
        self.atomic_u32(offset).store(value, order::STORE);
    }

    #[inline]
    fn load_u32_relaxed(&self, offset: usize) -> u32 {
        self.atomic_u32(offset).load(order::RELAXED)
    }

    #[inline]
    fn load_u64_relaxed(&self, offset: usize) -> u64 {
        self.atomic_u64(offset).load(order::RELAXED)
    }

    #[inline]
    fn load_u64(&self, offset: usize) -> u64 {
        self.atomic_u64(offset).load(order::LOAD)
    }

    #[inline]
    fn store_u64(&self, offset: usize, value: u64) {
        self.atomic_u64(offset).store(value, order::STORE);
    }

    #[inline]
    fn fetch_add_u64(&self, offset: usize, delta: u64) -> u64 {
        self.atomic_u64(offset).fetch_add(delta, order::ADD)
    }

    #[inline]
    unsafe fn zero_u32(&self, offset: usize, count: usize) {
        let address = self.at(offset, count * 4);
        // SAFETY: in bounds, and the caller has the index to itself.
        unsafe { core::slice::from_raw_parts_mut(address, count * 4) }.fill(0);
    }

    #[inline(always)]
    unsafe fn load_u32_in(&self, offset: usize) -> u32 {
        // SAFETY: in bounds by the caller's contract; aligned and accessed
        // only atomically as for `atomic_u32`.
        unsafe { AtomicU32::from_ptr(self.at_in(offset, 4).cast()) }.load(order::LOAD)
    }

    #[inline(always)]
    unsafe fn cas_u32_in(&self, offset: usize, current: u32, new: u32) -> Result<u32, u32> {
        // SAFETY: as for `load_u32_in`.
        unsafe { AtomicU32::from_ptr(self.at_in(offset, 4).cast()) }.compare_exchange(
            current,
            new,
            order::CAS,
            order::CAS_FAILED,
        )
    }

    #[inline(always)]
    fn prefetch(&self, offset: usize) {
        super::lanes::prefetch(self.base.wrapping_add(offset));
    }

    #[inline(always)]
    fn chunks(&self) -> usize {
        self.nchunks
    }

    #[inline(always)]
    fn chunk_len(&self, chunk: usize) -> usize {
        assert!(chunk < self.nchunks, "chunk {chunk} past the chunks");
        // SAFETY: the constructor's contract on the array.
        unsafe { *self.chunk_lens.add(chunk) }
    }

    #[inline(always)]
    unsafe fn chunk_used(&self, chunk: usize) -> u64 {
        // SAFETY: a chunk holds at least its 8-byte header, aligned to 8,
        // which only its one writer stores.
        unsafe { self.chunk_at(chunk, 0, 8).cast::<u64>().read() }
    }

    #[inline(always)]
    unsafe fn set_chunk_used(&self, chunk: usize, used: u64) {
        // SAFETY: as for `chunk_used`, by the chunk's writer.
        unsafe { self.chunk_at(chunk, 0, 8).cast::<u64>().write(used) }
    }

    #[inline(always)]
    unsafe fn record(&self, chunk: usize, byte: usize, len: usize) -> &[u8] {
        // SAFETY: the caller's contract.
        unsafe { core::slice::from_raw_parts(self.chunk_at(chunk, byte, len), len) }
    }

    #[inline(always)]
    unsafe fn record_mut(&self, chunk: usize, byte: usize, len: usize) -> &mut [u8] {
        // SAFETY: the caller's contract.
        unsafe { core::slice::from_raw_parts_mut(self.chunk_at(chunk, byte, len), len) }
    }

    #[inline(always)]
    unsafe fn load_next(&self, chunk: usize, byte: usize) -> u32 {
        // SAFETY: the caller's contract; records start at multiples of 8,
        // their next field 4 bytes in, so the word is aligned.
        unsafe { self.chunk_at(chunk, byte, 4).cast::<u32>().read() }
    }

    #[inline(always)]
    unsafe fn store_next(&self, chunk: usize, byte: usize, value: u32) {
        // SAFETY: as for `load_next`.
        unsafe { self.chunk_at(chunk, byte, 4).cast::<u32>().write(value) }
    }

    #[inline(always)]
    fn prefetch_record(&self, chunk: usize, byte: usize) {
        if chunk < self.nchunks {
            // SAFETY: the chunk exists; the address is only a hint.
            let base = unsafe { *self.chunk_bases.add(chunk) };
            super::lanes::prefetch(base.wrapping_add(byte));
        }
    }
}
