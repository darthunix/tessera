//! Whole-word sums and extremes of int4 values under a row mask.
//!
//! `mask` names the rows to aggregate: selected and non-NULL, computed by
//! the caller (for Datum blocks from [`super::non_null_bits`]). A NULL row's
//! placeholder value is loaded but masked away. A full mask skips the
//! masking; the extremes are meaningful only for a nonzero mask.

use core::arch::aarch64::{
    int32x4_t, int64x2_t, vaddq_s64, vaddvq_s64, vandq_s32, vbslq_s32, vdupq_n_s32, vdupq_n_s64,
    vld1q_s32, vmaxq_s32, vmaxvq_s32, vminq_s32, vminvq_s32, vpadalq_s32, vreinterpretq_s32_u32,
};

use super::{byte_weights, lane_masks, load_datums};

/// Sum of the masked rows of a dense block.
pub fn sum_dense(values: &[i32; 64], mask: u64) -> i64 {
    const { assert!(cfg!(target_feature = "neon")) }
    // SAFETY: NEON is enabled for this compilation (asserted above).
    unsafe { sum_dense_lanes(values, mask) }
}

/// Sum of the masked rows of a Datum block, as int4 values.
pub fn sum_datum(values: &[u64; 64], mask: u64) -> i64 {
    const { assert!(cfg!(target_feature = "neon")) }
    // SAFETY: as in sum_dense.
    unsafe { sum_datum_lanes(values, mask) }
}

/// Least masked row of a dense block; `i32::MAX` for an empty mask.
pub fn min_dense(values: &[i32; 64], mask: u64) -> i32 {
    const { assert!(cfg!(target_feature = "neon")) }
    // SAFETY: as in sum_dense.
    unsafe { min_dense_lanes(values, mask) }
}

/// Greatest masked row of a dense block; `i32::MIN` for an empty mask.
pub fn max_dense(values: &[i32; 64], mask: u64) -> i32 {
    const { assert!(cfg!(target_feature = "neon")) }
    // SAFETY: as in sum_dense.
    unsafe { max_dense_lanes(values, mask) }
}

/// Least masked row of a Datum block; `i32::MAX` for an empty mask.
pub fn min_datum(values: &[u64; 64], mask: u64) -> i32 {
    const { assert!(cfg!(target_feature = "neon")) }
    // SAFETY: as in sum_dense.
    unsafe { min_datum_lanes(values, mask) }
}

/// Greatest masked row of a Datum block; `i32::MIN` for an empty mask.
pub fn max_datum(values: &[u64; 64], mask: u64) -> i32 {
    const { assert!(cfg!(target_feature = "neon")) }
    // SAFETY: as in sum_dense.
    unsafe { max_datum_lanes(values, mask) }
}

#[target_feature(enable = "neon")]
fn sum_dense_lanes(values: &[i32; 64], mask: u64) -> i64 {
    // SAFETY: `group` is below 16, so the four lanes read end within the array.
    let load = |group: usize| unsafe { vld1q_s32(values.as_ptr().add(group * 4)) };
    sum_lanes(load, mask)
}

#[target_feature(enable = "neon")]
fn sum_datum_lanes(values: &[u64; 64], mask: u64) -> i64 {
    // SAFETY: `group` is below 16.
    let load = |group: usize| unsafe { load_datums(values, group) };
    sum_lanes(load, mask)
}

#[target_feature(enable = "neon")]
fn min_dense_lanes(values: &[i32; 64], mask: u64) -> i32 {
    // SAFETY: as in sum_dense_lanes.
    let load = |group: usize| unsafe { vld1q_s32(values.as_ptr().add(group * 4)) };
    vminvq_s32(extreme_lanes(load, mask, i32::MAX, |a, b| vminq_s32(a, b)))
}

#[target_feature(enable = "neon")]
fn max_dense_lanes(values: &[i32; 64], mask: u64) -> i32 {
    // SAFETY: as in sum_dense_lanes.
    let load = |group: usize| unsafe { vld1q_s32(values.as_ptr().add(group * 4)) };
    vmaxvq_s32(extreme_lanes(load, mask, i32::MIN, |a, b| vmaxq_s32(a, b)))
}

#[target_feature(enable = "neon")]
fn min_datum_lanes(values: &[u64; 64], mask: u64) -> i32 {
    // SAFETY: `group` is below 16.
    let load = |group: usize| unsafe { load_datums(values, group) };
    vminvq_s32(extreme_lanes(load, mask, i32::MAX, |a, b| vminq_s32(a, b)))
}

#[target_feature(enable = "neon")]
fn max_datum_lanes(values: &[u64; 64], mask: u64) -> i32 {
    // SAFETY: `group` is below 16.
    let load = |group: usize| unsafe { load_datums(values, group) };
    vmaxvq_s32(extreme_lanes(load, mask, i32::MIN, |a, b| vmaxq_s32(a, b)))
}

/// Pairwise widening adds into two accumulators keep the dependency chains
/// short; masked lanes are zeroed first.
#[inline]
#[target_feature(enable = "neon")]
fn sum_lanes(load: impl Fn(usize) -> int32x4_t, mask: u64) -> i64 {
    let mut even: int64x2_t = vdupq_n_s64(0);
    let mut odd: int64x2_t = vdupq_n_s64(0);
    if mask == u64::MAX {
        for pair in 0..8 {
            even = vpadalq_s32(even, load(pair * 2));
            odd = vpadalq_s32(odd, load(pair * 2 + 1));
        }
    } else {
        let weights = byte_weights();
        for byte in 0..8 {
            let (first, second) = lane_masks((mask >> (byte * 8)) as u8, weights);
            even = vpadalq_s32(
                even,
                vandq_s32(load(byte * 2), vreinterpretq_s32_u32(first)),
            );
            odd = vpadalq_s32(
                odd,
                vandq_s32(load(byte * 2 + 1), vreinterpretq_s32_u32(second)),
            );
        }
    }
    vaddvq_s64(vaddq_s64(even, odd))
}

/// Lane-wise `combine` over the groups, with masked lanes replaced by the
/// operation's identity; the caller reduces the four lanes.
#[inline]
#[target_feature(enable = "neon")]
fn extreme_lanes(
    load: impl Fn(usize) -> int32x4_t,
    mask: u64,
    identity: i32,
    combine: impl Fn(int32x4_t, int32x4_t) -> int32x4_t,
) -> int32x4_t {
    let identity = vdupq_n_s32(identity);
    let mut acc = identity;
    if mask == u64::MAX {
        for group in 0..16 {
            acc = combine(acc, load(group));
        }
    } else {
        let weights = byte_weights();
        for byte in 0..8 {
            let (first, second) = lane_masks((mask >> (byte * 8)) as u8, weights);
            acc = combine(acc, vbslq_s32(first, load(byte * 2), identity));
            acc = combine(acc, vbslq_s32(second, load(byte * 2 + 1), identity));
        }
    }
    acc
}
