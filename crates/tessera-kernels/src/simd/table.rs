//! Operations of the hash table's vertical probe.
//!
//! The contract speaks of addresses, not of lanes, so that an x86 version
//! (plan item 3.9) is a second file with the same entry point.
//! Correspondences:
//!
//! - prefetch: `prfm pldl1keep` here, `_mm_prefetch(_MM_HINT_T0)` there.
//!
//! Array comparisons in masks were tried and removed: the probe gathers a
//! candidate's fields with scalar loads from scattered records into
//! arrays, and vector loads of arrays just filled by 32-bit stores cannot
//! take their data from those stores; with the comparisons the probe of a
//! table in L1 took twice the cycles of one that compares each field as
//! it is read.

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
