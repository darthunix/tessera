//! A call of the crate allocates nothing: packing, under an allocator
//! that counts what it gives while the test watches. The binary holds
//! this one test, so that no other thread allocates meanwhile.
#![allow(
    unsafe_code,
    clippy::unwrap_used,
    reason = "an allocator is unsafe to write; a test reports a failure by panicking"
)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct Counting;

static WATCHING: AtomicBool = AtomicBool::new(false);
static GIVEN: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every call goes to the system's allocator, as it came.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if WATCHING.load(Ordering::Relaxed) {
            GIVEN.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: the caller's contract, passed on.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller's contract, passed on.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// A chunk of `count` records of `len` bytes, each its length in words
/// and a word that grows by record.
fn chunk(count: usize, len: usize) -> Vec<u8> {
    let mut bytes = vec![0_u8; 8 + count * len];
    let total = bytes.len() as u64;
    bytes[..8].copy_from_slice(&total.to_ne_bytes());
    for record in bytes[8..].chunks_exact_mut(len) {
        record[relhash::RECORD_SIZE_OFFSET..relhash::RECORD_SIZE_OFFSET + 4]
            .copy_from_slice(&((len / 8) as u32).to_ne_bytes());
    }
    for (number, record) in bytes[8..].chunks_exact_mut(len).enumerate() {
        record[len - 4..].copy_from_slice(&(number as u32).to_ne_bytes());
    }
    bytes
}

#[test]
fn packing_allocates_nothing() {
    // Records of a pass's lanes and of more.
    for len in [24, 304] {
        let original = chunk(100, len);
        let mut out = vec![0_u8; original.len()];
        WATCHING.store(true, Ordering::Relaxed);
        let packed = relspill::pack(&original, &mut out);
        WATCHING.store(false, Ordering::Relaxed);
        assert!(packed.is_some(), "records of {len} bytes pack");
        assert_eq!(GIVEN.load(Ordering::Relaxed), 0, "records of {len} bytes");
    }
}
