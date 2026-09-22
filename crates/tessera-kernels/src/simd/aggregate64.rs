//! Whole-word extremes of int8 values under a row mask.
//!
//! `mask` names the rows to aggregate: selected and non-NULL, computed by
//! the caller (for Datum blocks from [`super::non_null_bits`]). A NULL
//! row's placeholder value is loaded but masked away. A full mask skips
//! the masking; the extremes are meaningful only for a nonzero mask. NEON
//! has no 64-bit lane minimum or maximum, so a lane-wise extreme is a
//! compare and a select, and the two lanes of the result reduce in
//! scalar code; four accumulators keep the chain short, as for int4.

use core::arch::aarch64::{
    int64x2_t, uint32x4_t, uint64x2_t, vbslq_s64, vcgtq_s64, vdupq_n_s64, vget_low_s32,
    vgetq_lane_s64, vld1q_s64, vmovl_high_s32, vmovl_s32, vreinterpretq_s32_u32,
    vreinterpretq_u64_s64,
};

use super::{byte_weights, lane_masks};

/// Least masked row of a dense block; `i64::MAX` for an empty mask.
#[inline]
pub fn min_dense64(values: &[i64; 64], mask: u64) -> i64 {
    const { assert!(cfg!(target_feature = "neon")) }
    // SAFETY: NEON is enabled for this compilation (asserted above).
    unsafe { min_dense_lanes(values, mask) }
}

/// Greatest masked row of a dense block; `i64::MIN` for an empty mask.
#[inline]
pub fn max_dense64(values: &[i64; 64], mask: u64) -> i64 {
    const { assert!(cfg!(target_feature = "neon")) }
    // SAFETY: as in min_dense64.
    unsafe { max_dense_lanes(values, mask) }
}

/// Least masked row of a Datum block; `i64::MAX` for an empty mask.
#[inline]
pub fn min_datum64(values: &[u64; 64], mask: u64) -> i64 {
    const { assert!(cfg!(target_feature = "neon")) }
    // SAFETY: as in min_dense64.
    unsafe { min_datum_lanes(values, mask) }
}

/// Greatest masked row of a Datum block; `i64::MIN` for an empty mask.
#[inline]
pub fn max_datum64(values: &[u64; 64], mask: u64) -> i64 {
    const { assert!(cfg!(target_feature = "neon")) }
    // SAFETY: as in min_dense64.
    unsafe { max_datum_lanes(values, mask) }
}

#[target_feature(enable = "neon")]
fn min_dense_lanes(values: &[i64; 64], mask: u64) -> i64 {
    // SAFETY: `pair` is below 32, so the two lanes read end within the array.
    let load = |pair: usize| unsafe { vld1q_s64(values.as_ptr().add(pair * 2)) };
    reduce_min(extreme_lanes(load, mask, i64::MAX, |a, b| least(a, b)))
}

#[target_feature(enable = "neon")]
fn max_dense_lanes(values: &[i64; 64], mask: u64) -> i64 {
    // SAFETY: as in min_dense_lanes.
    let load = |pair: usize| unsafe { vld1q_s64(values.as_ptr().add(pair * 2)) };
    reduce_max(extreme_lanes(load, mask, i64::MIN, |a, b| greatest(a, b)))
}

#[target_feature(enable = "neon")]
fn min_datum_lanes(values: &[u64; 64], mask: u64) -> i64 {
    // SAFETY: `pair` is below 32; a Datum holding an int8 is its bits.
    let load = |pair: usize| unsafe { vld1q_s64(values.as_ptr().cast::<i64>().add(pair * 2)) };
    reduce_min(extreme_lanes(load, mask, i64::MAX, |a, b| least(a, b)))
}

#[target_feature(enable = "neon")]
fn max_datum_lanes(values: &[u64; 64], mask: u64) -> i64 {
    // SAFETY: as in min_datum_lanes.
    let load = |pair: usize| unsafe { vld1q_s64(values.as_ptr().cast::<i64>().add(pair * 2)) };
    reduce_max(extreme_lanes(load, mask, i64::MIN, |a, b| greatest(a, b)))
}

/// Lane-wise minimum: where `a > b` take `b`.
#[inline]
#[target_feature(enable = "neon")]
fn least(a: int64x2_t, b: int64x2_t) -> int64x2_t {
    vbslq_s64(vcgtq_s64(a, b), b, a)
}

/// Lane-wise maximum: where `a > b` take `a`.
#[inline]
#[target_feature(enable = "neon")]
fn greatest(a: int64x2_t, b: int64x2_t) -> int64x2_t {
    vbslq_s64(vcgtq_s64(a, b), a, b)
}

#[inline]
#[target_feature(enable = "neon")]
fn reduce_min(lanes: int64x2_t) -> i64 {
    vgetq_lane_s64::<0>(lanes).min(vgetq_lane_s64::<1>(lanes))
}

#[inline]
#[target_feature(enable = "neon")]
fn reduce_max(lanes: int64x2_t) -> i64 {
    vgetq_lane_s64::<0>(lanes).max(vgetq_lane_s64::<1>(lanes))
}

/// Four rows of lane masks widened to two pairs: an all-ones 32-bit lane
/// sign-extends to an all-ones 64-bit one.
#[inline]
#[target_feature(enable = "neon")]
fn widen(rows: uint32x4_t) -> (uint64x2_t, uint64x2_t) {
    let rows = vreinterpretq_s32_u32(rows);
    (
        vreinterpretq_u64_s64(vmovl_s32(vget_low_s32(rows))),
        vreinterpretq_u64_s64(vmovl_high_s32(rows)),
    )
}

/// Lane-wise `combine` over the pairs in four accumulators, with masked
/// lanes replaced by the operation's identity; the caller reduces the two
/// lanes of the result.
#[inline]
#[target_feature(enable = "neon")]
fn extreme_lanes(
    load: impl Fn(usize) -> int64x2_t,
    mask: u64,
    identity: i64,
    combine: impl Fn(int64x2_t, int64x2_t) -> int64x2_t,
) -> int64x2_t {
    let identity = vdupq_n_s64(identity);
    let (mut a, mut b, mut c, mut d) = (identity, identity, identity, identity);
    if mask == u64::MAX {
        for step in 0..8 {
            a = combine(a, load(step * 4));
            b = combine(b, load(step * 4 + 1));
            c = combine(c, load(step * 4 + 2));
            d = combine(d, load(step * 4 + 3));
        }
    } else {
        let weights = byte_weights();
        for step in 0..8 {
            let (first, second) = lane_masks((mask >> (step * 8)) as u8, weights);
            let (m0, m1) = widen(first);
            let (m2, m3) = widen(second);
            a = combine(a, vbslq_s64(m0, load(step * 4), identity));
            b = combine(b, vbslq_s64(m1, load(step * 4 + 1), identity));
            c = combine(c, vbslq_s64(m2, load(step * 4 + 2), identity));
            d = combine(d, vbslq_s64(m3, load(step * 4 + 3), identity));
        }
    }
    combine(combine(a, b), combine(c, d))
}
