//! Whole-word operations of the hash table's vertical probe.
//!
//! The contract speaks of arrays of a word's 64 rows and of prefetch
//! hints, not of lanes, so that an AVX2 version (plan item 3.9) is a
//! second file with the same entry points. Correspondences:
//!
//! - prefetch: `prfm pldl1keep` here, `_mm_prefetch(_MM_HINT_T0)` there;
//! - equality of u32: `vceqq_u32` / `_mm256_cmpeq_epi32`;
//! - equality of i64: `vceqq_s64` / `_mm256_cmpeq_epi64`;
//! - lanes to bits: here the lane weights of a 16-row quarter, `and` and a
//!   horizontal add (`vaddvq_u32`), i64 results narrowed to u32 first with
//!   `vmovn_u64`; there `_mm256_movemask_ps` and `_mm256_movemask_pd`, one
//!   bit per lane directly.

use core::arch::aarch64::{
    uint32x4_t, vaddvq_u32, vandq_u32, vceqq_s64, vceqq_u32, vcombine_u32, vld1q_s64, vld1q_u32,
    vmovn_u64, vorrq_u32,
};
use core::arch::asm;

use super::LANE_WEIGHTS;

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

/// Bit `i` set where `a[i] == b[i]`.
#[inline]
pub fn eq_mask_u32(a: &[u32; 64], b: &[u32; 64]) -> u64 {
    const { assert!(cfg!(target_feature = "neon")) }
    // SAFETY: NEON is enabled for this compilation (asserted above).
    unsafe { eq_lanes_u32(a, b) }
}

/// Bit `i` set where `a[i] == b[i]`.
#[inline]
pub fn eq_mask_i64(a: &[i64; 64], b: &[i64; 64]) -> u64 {
    const { assert!(cfg!(target_feature = "neon")) }
    // SAFETY: NEON is enabled for this compilation (asserted above).
    unsafe { eq_lanes_i64(a, b) }
}

/// The bits of a quarter of 16 rows from its four groups' lane masks.
#[inline]
#[target_feature(enable = "neon")]
fn quarter_bits(groups: [uint32x4_t; 4], weights: &[uint32x4_t; 4]) -> u64 {
    let weighted = vorrq_u32(
        vorrq_u32(
            vandq_u32(groups[0], weights[0]),
            vandq_u32(groups[1], weights[1]),
        ),
        vorrq_u32(
            vandq_u32(groups[2], weights[2]),
            vandq_u32(groups[3], weights[3]),
        ),
    );
    u64::from(vaddvq_u32(weighted))
}

#[inline]
#[target_feature(enable = "neon")]
fn eq_lanes_u32(a: &[u32; 64], b: &[u32; 64]) -> u64 {
    // SAFETY: four rows of four weights.
    let weights = LANE_WEIGHTS.map(|row| unsafe { vld1q_u32(row.as_ptr()) });
    let mut bits = 0;
    for quarter in 0..4 {
        let groups = core::array::from_fn(|group| {
            let at = quarter * 16 + group * 4;
            // SAFETY: `at + 4 <= 64`, within both arrays.
            unsafe { vceqq_u32(vld1q_u32(a.as_ptr().add(at)), vld1q_u32(b.as_ptr().add(at))) }
        });
        bits |= quarter_bits(groups, &weights) << (quarter * 16);
    }
    bits
}

#[inline]
#[target_feature(enable = "neon")]
fn eq_lanes_i64(a: &[i64; 64], b: &[i64; 64]) -> u64 {
    // SAFETY: four rows of four weights.
    let weights = LANE_WEIGHTS.map(|row| unsafe { vld1q_u32(row.as_ptr()) });
    let mut bits = 0;
    for quarter in 0..4 {
        let groups = core::array::from_fn(|group| {
            let at = quarter * 16 + group * 4;
            // SAFETY: `at + 4 <= 64`, within both arrays; two lanes of two
            // equal results narrow to four u32 lanes, all ones or zero.
            unsafe {
                let low = vceqq_s64(vld1q_s64(a.as_ptr().add(at)), vld1q_s64(b.as_ptr().add(at)));
                let high = vceqq_s64(
                    vld1q_s64(a.as_ptr().add(at + 2)),
                    vld1q_s64(b.as_ptr().add(at + 2)),
                );
                vcombine_u32(vmovn_u64(low), vmovn_u64(high))
            }
        });
        bits |= quarter_bits(groups, &weights) << (quarter * 16);
    }
    bits
}
