//! The prefetch hint of the vertical probe: `prfm pldl1keep` on AArch64,
//! nothing elsewhere (other targets and Miri), in the manner of the hash
//! kernels' `bulk_op`. The table keeps its own hint, so that it needs no
//! module of the kernels; on x86-64 it would be `_mm_prefetch(_MM_HINT_T0)`.
//!
//! Array comparisons in masks were tried and removed: the probe gathers a
//! candidate's fields with scalar loads from scattered records into
//! arrays, and vector loads of arrays just filled by 32-bit stores cannot
//! take their data from those stores; with the comparisons the probe of a
//! table in L1 took twice the cycles of one that compares each field as
//! it is read.

#[cfg(all(target_arch = "aarch64", not(miri)))]
use core::arch::asm;

/// Ask for the cache line at `address` to be loaded for reading. A hint:
/// it never faults, whatever the address, and changes no state a program
/// can observe.
#[cfg(all(target_arch = "aarch64", not(miri)))]
#[inline(always)]
pub(super) fn prefetch(address: *const u8) {
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

/// A prefetch hint where none is implemented: nothing.
#[cfg(not(all(target_arch = "aarch64", not(miri))))]
#[inline(always)]
pub(super) fn prefetch(_address: *const u8) {}
