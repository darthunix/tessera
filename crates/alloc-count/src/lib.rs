//! The system's allocator, counting each thread's allocations, so that a
//! test sees that a call allocates nothing. A test binary that links this
//! crate allocates through it; the crates under test stay free of the
//! unsafe code an allocator takes.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

std::thread_local! {
    /// The allocations this thread made.
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

/// The system's allocator, counting each thread's allocations.
struct Counting;

// SAFETY: every call is the system allocator's, with the caller's
// arguments.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // A thread that is ending may no longer count; its allocations
        // are no test's.
        let _ = ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
        // SAFETY: the caller's contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: the caller's contract.
        unsafe { System.dealloc(pointer, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// The allocations this thread has made so far.
pub fn allocations() -> usize {
    ALLOCATIONS.with(Cell::get)
}

#[cfg(test)]
mod tests {
    use super::allocations;

    #[test]
    fn the_allocations_of_a_thread_are_counted() {
        let before = allocations();
        let words = std::hint::black_box(vec![0_u64; 4]);
        assert_eq!(allocations(), before + 1);
        drop(words);
    }
}
