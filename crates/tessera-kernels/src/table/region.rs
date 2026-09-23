//! The memory a table lives in, addressed by offsets from its start.
//!
//! A table never holds a process address: the region is handed to every
//! operation as a base pointer and a length, and everything inside it refers
//! to other parts by offset, so the same bytes work after `repalloc` moved
//! them and when several processes map them at different addresses.
//! Counters and bucket heads that several participants may change at once
//! are read and written only through the atomic operations here; record
//! bytes are plain memory, written before a record is published and never
//! after. [`RawRegion`] is the one implementation, over raw pointers; the
//! trait is the seam for a model region under loom, and grows with the
//! operations the table needs.
//!
//! Offsets are validated against the header by the table before any call,
//! so a violation here is a bug: the methods assert it instead of returning
//! an error.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Bytes a table reads and writes by offset.
pub(super) trait Region {
    /// Read a 32-bit word with acquire ordering.
    fn load_u32(&self, offset: usize) -> u32;

    /// Write a 32-bit word with release ordering.
    fn store_u32(&self, offset: usize, value: u32);

    /// Replace a 32-bit word if it still holds `current`: `Ok` with the
    /// value replaced, `Err` with the value found (acquire-release).
    fn cas_u32(&self, offset: usize, current: u32, new: u32) -> Result<u32, u32>;

    /// Read a 64-bit word with acquire ordering.
    fn load_u64(&self, offset: usize) -> u64;

    /// Write a 64-bit word with release ordering.
    fn store_u64(&self, offset: usize, value: u64);

    /// Replace a 64-bit word if it still holds `current`, as [`Self::cas_u32`].
    fn cas_u64(&self, offset: usize, current: u64, new: u64) -> Result<u64, u64>;

    /// Add to a 64-bit word (acquire-release) and return its previous value.
    fn fetch_add_u64(&self, offset: usize, delta: u64) -> u64;

    /// Borrow `len` bytes at `offset` for reading.
    ///
    /// # Safety
    ///
    /// Nothing writes these bytes while the slice lives: the range holds a
    /// published record, or the caller has exclusive use of the region.
    unsafe fn bytes(&self, offset: usize, len: usize) -> &[u8];

    /// Borrow `len` bytes at `offset` for writing.
    ///
    /// # Safety
    ///
    /// Nothing else reads or writes these bytes while the slice lives: the
    /// range is reserved and not yet published, or the caller has exclusive
    /// use of the region.
    #[allow(clippy::mut_from_ref)]
    unsafe fn bytes_mut(&self, offset: usize, len: usize) -> &mut [u8];
}

/// A region over the caller's memory.
#[derive(Debug)]
pub(super) struct RawRegion {
    base: *mut u8,
    len: usize,
}

impl RawRegion {
    /// Address `len` bytes at `base`.
    ///
    /// # Safety
    ///
    /// `base` is aligned to 8 and valid for reads and writes of `len` bytes
    /// for as long as the region is used, and during that time the bytes are
    /// accessed only through tables over this region, in one process or in
    /// several, each with a mapping of its own.
    pub(super) unsafe fn new(base: *mut u8, len: usize) -> Self {
        debug_assert!(base.addr().is_multiple_of(8));
        Self { base, len }
    }

    /// The address of `size` bytes at `offset`, both checked.
    fn at(&self, offset: usize, size: usize) -> *mut u8 {
        let end = offset.checked_add(size).expect("region offset overflows");
        assert!(end <= self.len, "region access past its end");
        // SAFETY: `offset` is within the `len` bytes the constructor promised.
        unsafe { self.base.add(offset) }
    }

    /// The 32-bit atomic at `offset`, which must be a multiple of 4.
    fn atomic_u32(&self, offset: usize) -> &AtomicU32 {
        let address = self.at(offset, 4);
        debug_assert!(address.addr().is_multiple_of(4));
        // SAFETY: the address is in bounds and aligned to 4 (the base is
        // aligned to 8 and offsets of 32-bit words are multiples of 4); the
        // words reached this way are accessed only atomically by every
        // table over the region, as the constructor's contract requires.
        unsafe { AtomicU32::from_ptr(address.cast()) }
    }

    /// The 64-bit atomic at `offset`, which must be a multiple of 8.
    fn atomic_u64(&self, offset: usize) -> &AtomicU64 {
        let address = self.at(offset, 8);
        debug_assert!(address.addr().is_multiple_of(8));
        // SAFETY: as for the 32-bit atomics, with alignment 8.
        unsafe { AtomicU64::from_ptr(address.cast()) }
    }
}

impl Region for RawRegion {
    fn load_u32(&self, offset: usize) -> u32 {
        self.atomic_u32(offset).load(Ordering::Acquire)
    }

    fn store_u32(&self, offset: usize, value: u32) {
        self.atomic_u32(offset).store(value, Ordering::Release);
    }

    fn cas_u32(&self, offset: usize, current: u32, new: u32) -> Result<u32, u32> {
        self.atomic_u32(offset)
            .compare_exchange(current, new, Ordering::AcqRel, Ordering::Acquire)
    }

    fn load_u64(&self, offset: usize) -> u64 {
        self.atomic_u64(offset).load(Ordering::Acquire)
    }

    fn store_u64(&self, offset: usize, value: u64) {
        self.atomic_u64(offset).store(value, Ordering::Release);
    }

    fn cas_u64(&self, offset: usize, current: u64, new: u64) -> Result<u64, u64> {
        self.atomic_u64(offset)
            .compare_exchange(current, new, Ordering::AcqRel, Ordering::Acquire)
    }

    fn fetch_add_u64(&self, offset: usize, delta: u64) -> u64 {
        self.atomic_u64(offset).fetch_add(delta, Ordering::AcqRel)
    }

    unsafe fn bytes(&self, offset: usize, len: usize) -> &[u8] {
        let address = self.at(offset, len);
        // SAFETY: the range is in bounds, and the caller promises that nothing
        // writes it while the slice lives.
        unsafe { core::slice::from_raw_parts(address, len) }
    }

    unsafe fn bytes_mut(&self, offset: usize, len: usize) -> &mut [u8] {
        let address = self.at(offset, len);
        // SAFETY: the range is in bounds, and the caller promises that nothing
        // else reads or writes it while the slice lives.
        unsafe { core::slice::from_raw_parts_mut(address, len) }
    }
}
