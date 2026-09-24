//! Whole-word operations of the hash table's vertical probe.
//!
//! The contract speaks of arrays of a word's 64 rows and of prefetch
//! hints, not of lanes, so that an AVX2 version (plan item 3.9) is a
//! second file with the same entry points. Correspondences:
//!
//! - prefetch: `prfm pldl1keep` here, `_mm_prefetch(_MM_HINT_T0)` there.

use core::arch::asm;

/// Ask for the cache line at `address` to be loaded for reading. A hint:
/// it never faults, whatever the address, and changes no state a program
/// can observe.
#[inline(always)]
pub fn prefetch(address: *const u8) {
    // SAFETY: `prfm` reads no memory architecturally and does not fault
    // on any address; it touches no register but its operand.
    unsafe {
        asm!(
            "prfm pldl1keep, [{address}]",
            address = in(reg) address,
            options(nostack, preserves_flags, readonly)
        );
    }
}
