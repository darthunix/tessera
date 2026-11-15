//! What the tests share: a check that the crate's calls allocate
//! nothing, as "No memory allocated" promises. With `--cfg
//! count_allocations` the tests link the allocator of `alloc-count`,
//! which counts each thread's allocations; without it, as in the
//! crate's package alone, no allocation is counted.

#[cfg(count_allocations)]
use alloc_count::allocations;

/// No allocation is counted without the allocator of `alloc-count`.
#[cfg(not(count_allocations))]
fn allocations() -> usize {
    0
}

/// Run `calls`, calls of the crate alone, and check that they allocate
/// nothing, also when they fail.
#[track_caller]
pub(crate) fn quiet<T>(calls: impl FnOnce() -> T) -> T {
    let before = allocations();
    let out = calls();
    assert_eq!(allocations(), before, "a call of the crate took memory");
    out
}

/// An allocation under [`quiet`] fails the test.
#[cfg(count_allocations)]
#[test]
#[should_panic(expected = "took memory")]
fn an_allocation_of_a_call_is_seen() {
    quiet(|| std::hint::black_box(vec![0_u8; 8]));
}
