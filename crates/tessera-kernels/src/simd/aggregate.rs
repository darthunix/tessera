//! Whole-word sums and extremes of int4 values under a row mask.
//!
//! `mask` names the rows to aggregate: selected and non-NULL, computed by
//! the caller (for Datum blocks from [`super::non_null_bits`]). A NULL row's
//! placeholder value is loaded but masked away. A full mask skips the
//! masking; the extremes are meaningful only for a nonzero mask.

use core::arch::aarch64::{
    int32x4_t, int64x2_t, vaddlvq_u8, vaddq_s64, vaddq_u8, vaddvq_s64, vandq_s32, vandq_u8,
    vbslq_s32, vceqzq_u8, vcombine_u8, vdup_n_u8, vdupq_n_s32, vdupq_n_s64, vdupq_n_u8, vld1q_s32,
    vld1q_u8, vmaxq_s32, vmaxvq_s32, vminq_s32, vminvq_s32, vpadalq_s32, vreinterpretq_s32_u32,
    vtst_u8,
};

use super::{byte_weights, lane_masks, load_datums};

// The public entry points are inlined into the generic loops of other
// crates, so that a block reaches them in registers; the feature-carrying
// kernels behind them stay out of line.

/// Selected non-NULL rows of a Datum block: the flags are compared with
/// zero and counted as bytes, so that no `addv` per eight rows is needed.
#[inline]
pub fn count_datum(isnull: &[bool; 64], selected: u64) -> usize {
    const { assert!(cfg!(target_feature = "neon")) }
    // SAFETY: NEON is enabled for this compilation (asserted above).
    unsafe { count_datum_lanes(isnull, selected) }
}

/// Sum of the masked rows of a dense block.
#[inline]
pub fn sum_dense(values: &[i32; 64], mask: u64) -> i64 {
    const { assert!(cfg!(target_feature = "neon")) }
    // SAFETY: NEON is enabled for this compilation (asserted above).
    unsafe { sum_dense_lanes(values, mask) }
}

/// Sum of the masked rows of a Datum block, as int4 values.
#[inline]
pub fn sum_datum(values: &[u64; 64], mask: u64) -> i64 {
    const { assert!(cfg!(target_feature = "neon")) }
    // SAFETY: as in sum_dense.
    unsafe { sum_datum_lanes(values, mask) }
}

/// Least masked row of a dense block; `i32::MAX` for an empty mask.
#[inline]
pub fn min_dense(values: &[i32; 64], mask: u64) -> i32 {
    const { assert!(cfg!(target_feature = "neon")) }
    // SAFETY: as in sum_dense.
    unsafe { min_dense_lanes(values, mask) }
}

/// Greatest masked row of a dense block; `i32::MIN` for an empty mask.
#[inline]
pub fn max_dense(values: &[i32; 64], mask: u64) -> i32 {
    const { assert!(cfg!(target_feature = "neon")) }
    // SAFETY: as in sum_dense.
    unsafe { max_dense_lanes(values, mask) }
}

/// Least masked row of a Datum block; `i32::MAX` for an empty mask.
#[inline]
pub fn min_datum(values: &[u64; 64], mask: u64) -> i32 {
    const { assert!(cfg!(target_feature = "neon")) }
    // SAFETY: as in sum_dense.
    unsafe { min_datum_lanes(values, mask) }
}

/// Greatest masked row of a Datum block; `i32::MIN` for an empty mask.
#[inline]
pub fn max_datum(values: &[u64; 64], mask: u64) -> i32 {
    const { assert!(cfg!(target_feature = "neon")) }
    // SAFETY: as in sum_dense.
    unsafe { max_datum_lanes(values, mask) }
}

#[target_feature(enable = "neon")]
fn count_datum_lanes(isnull: &[bool; 64], selected: u64) -> usize {
    let weights = byte_weights();
    let one = vdupq_n_u8(1);
    // Per-lane counts stay at most four.
    let mut counts = vdupq_n_u8(0);
    for quarter in 0..4 {
        // SAFETY: a bool is one byte holding 0 or 1, and the sixteen bytes
        // read end within the array.
        let flags = unsafe { vld1q_u8(isnull.as_ptr().cast::<u8>().add(quarter * 16)) };
        let mut present = vceqzq_u8(flags);
        if selected != u64::MAX {
            let bits = (selected >> (quarter * 16)) as u16;
            let low = vtst_u8(vdup_n_u8(bits as u8), weights);
            let high = vtst_u8(vdup_n_u8((bits >> 8) as u8), weights);
            present = vandq_u8(present, vcombine_u8(low, high));
        }
        counts = vaddq_u8(counts, vandq_u8(present, one));
    }
    usize::from(vaddlvq_u8(counts))
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

/// Pairwise widening adds into four accumulators: a chain of four per
/// word instead of sixteen, which on an M5 Pro halves the cycles of a full
/// word; masked lanes are zeroed first.
#[inline]
#[target_feature(enable = "neon")]
fn sum_lanes(load: impl Fn(usize) -> int32x4_t, mask: u64) -> i64 {
    let zero: int64x2_t = vdupq_n_s64(0);
    let (mut a, mut b, mut c, mut d) = (zero, zero, zero, zero);
    if mask == u64::MAX {
        for step in 0..4 {
            a = vpadalq_s32(a, load(step * 4));
            b = vpadalq_s32(b, load(step * 4 + 1));
            c = vpadalq_s32(c, load(step * 4 + 2));
            d = vpadalq_s32(d, load(step * 4 + 3));
        }
    } else {
        let weights = byte_weights();
        for step in 0..4 {
            let (m0, m1) = lane_masks((mask >> (step * 16)) as u8, weights);
            let (m2, m3) = lane_masks((mask >> (step * 16 + 8)) as u8, weights);
            a = vpadalq_s32(a, vandq_s32(load(step * 4), vreinterpretq_s32_u32(m0)));
            b = vpadalq_s32(b, vandq_s32(load(step * 4 + 1), vreinterpretq_s32_u32(m1)));
            c = vpadalq_s32(c, vandq_s32(load(step * 4 + 2), vreinterpretq_s32_u32(m2)));
            d = vpadalq_s32(d, vandq_s32(load(step * 4 + 3), vreinterpretq_s32_u32(m3)));
        }
    }
    vaddvq_s64(vaddq_s64(vaddq_s64(a, b), vaddq_s64(c, d)))
}

/// Lane-wise `combine` over the groups in four accumulators, with masked
/// lanes replaced by the operation's identity; the caller reduces the four
/// lanes of the result.
#[inline]
#[target_feature(enable = "neon")]
fn extreme_lanes(
    load: impl Fn(usize) -> int32x4_t,
    mask: u64,
    identity: i32,
    combine: impl Fn(int32x4_t, int32x4_t) -> int32x4_t,
) -> int32x4_t {
    let identity = vdupq_n_s32(identity);
    let (mut a, mut b, mut c, mut d) = (identity, identity, identity, identity);
    if mask == u64::MAX {
        for step in 0..4 {
            a = combine(a, load(step * 4));
            b = combine(b, load(step * 4 + 1));
            c = combine(c, load(step * 4 + 2));
            d = combine(d, load(step * 4 + 3));
        }
    } else {
        let weights = byte_weights();
        for step in 0..4 {
            let (m0, m1) = lane_masks((mask >> (step * 16)) as u8, weights);
            let (m2, m3) = lane_masks((mask >> (step * 16 + 8)) as u8, weights);
            a = combine(a, vbslq_s32(m0, load(step * 4), identity));
            b = combine(b, vbslq_s32(m1, load(step * 4 + 1), identity));
            c = combine(c, vbslq_s32(m2, load(step * 4 + 2), identity));
            d = combine(d, vbslq_s32(m3, load(step * 4 + 3), identity));
        }
    }
    combine(combine(a, b), combine(c, d))
}
