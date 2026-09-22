//! Whole-word int8 addition and subtraction with overflow detection.
//!
//! Every lane is computed modulo 2^64 and stored; overflow is detected per
//! lane by comparing the wrapping result with the saturating one, kept
//! only for the lanes of `mask`, and reported once per word. NULL and
//! unselected lanes thus get initialized placeholders and never raise.
//! NEON has no 64-bit multiply or high multiply, so the multiplications
//! and divisions of a word stay in the caller's lane loop. A word is 32
//! pairs of lanes, loaded alike from dense storage and from Datums, which
//! hold an int8 whole.

use core::arch::aarch64::{
    int64x2_t, uint32x4_t, uint64x2_t, vaddq_s64, vandq_u64, vbicq_u64, vceqq_s64, vdupq_n_s64,
    vdupq_n_u64, vget_low_s32, vld1q_s64, vmaxvq_u32, vminvq_u32, vmovl_high_s32, vmovl_s32,
    vorrq_u64, vqaddq_s64, vqsubq_s64, vreinterpretq_s32_u32, vreinterpretq_u32_u64,
    vreinterpretq_u64_s64, vst1q_s64, vsubq_s64,
};
use std::mem::MaybeUninit;

use super::{byte_weights, lane_masks};
use crate::int64::Side;

/// `lhs + rhs` into `out`; true when a masked lane overflowed.
#[inline]
pub fn add64(lhs: Side<'_>, rhs: Side<'_>, mask: u64, out: &mut [MaybeUninit<i64>; 64]) -> bool {
    const { assert!(cfg!(target_feature = "neon")) }
    // SAFETY: NEON is enabled for this compilation (asserted above).
    unsafe { add_lanes(lhs, rhs, mask, out) }
}

/// `lhs - rhs` into `out`; true when a masked lane overflowed.
#[inline]
pub fn sub64(lhs: Side<'_>, rhs: Side<'_>, mask: u64, out: &mut [MaybeUninit<i64>; 64]) -> bool {
    const { assert!(cfg!(target_feature = "neon")) }
    // SAFETY: as in add64.
    unsafe { sub_lanes(lhs, rhs, mask, out) }
}

// The operation returns the wrapped result and the lanes that fit, all
// ones where no overflow occurred.

#[target_feature(enable = "neon")]
fn add_lanes(lhs: Side<'_>, rhs: Side<'_>, mask: u64, out: &mut [MaybeUninit<i64>; 64]) -> bool {
    dispatch(lhs, rhs, mask, out, |a, b| {
        let wrapped = vaddq_s64(a, b);
        (wrapped, vceqq_s64(wrapped, vqaddq_s64(a, b)))
    })
}

#[target_feature(enable = "neon")]
fn sub_lanes(lhs: Side<'_>, rhs: Side<'_>, mask: u64, out: &mut [MaybeUninit<i64>; 64]) -> bool {
    dispatch(lhs, rhs, mask, out, |a, b| {
        let wrapped = vsubq_s64(a, b);
        (wrapped, vceqq_s64(wrapped, vqsubq_s64(a, b)))
    })
}

/// A loader of the two int8 values of each pair of a dense block.
#[inline]
#[target_feature(enable = "neon")]
fn dense(values: &[i64; 64]) -> impl Fn(usize) -> int64x2_t + '_ {
    // SAFETY: `pair` is below 32, so the two lanes read end within the array.
    move |pair| unsafe { vld1q_s64(values.as_ptr().add(pair * 2)) }
}

/// A loader of the two int8 values of each pair of a Datum block: a Datum
/// holding an int8 is its bits.
#[inline]
#[target_feature(enable = "neon")]
fn datum(values: &[u64; 64]) -> impl Fn(usize) -> int64x2_t + '_ {
    // SAFETY: `pair` is below 32.
    move |pair| unsafe { vld1q_s64(values.as_ptr().cast::<i64>().add(pair * 2)) }
}

#[inline]
#[target_feature(enable = "neon")]
fn scalar(value: i64) -> impl Fn(usize) -> int64x2_t {
    let lanes = vdupq_n_s64(value);
    move |_| lanes
}

/// Choose the loaders for the operand kinds once per word.
#[inline]
#[target_feature(enable = "neon")]
fn dispatch(
    lhs: Side<'_>,
    rhs: Side<'_>,
    mask: u64,
    out: &mut [MaybeUninit<i64>; 64],
    op: impl Fn(int64x2_t, int64x2_t) -> (int64x2_t, uint64x2_t),
) -> bool {
    match (lhs, rhs) {
        (Side::Dense(a), Side::Dense(b)) => apply(mask, out, &op, dense(a), dense(b)),
        (Side::Dense(a), Side::Datum(b)) => apply(mask, out, &op, dense(a), datum(b)),
        (Side::Dense(a), Side::Scalar(b)) => apply(mask, out, &op, dense(a), scalar(b)),
        (Side::Datum(a), Side::Dense(b)) => apply(mask, out, &op, datum(a), dense(b)),
        (Side::Datum(a), Side::Datum(b)) => apply(mask, out, &op, datum(a), datum(b)),
        (Side::Datum(a), Side::Scalar(b)) => apply(mask, out, &op, datum(a), scalar(b)),
        (Side::Scalar(a), Side::Dense(b)) => apply(mask, out, &op, scalar(a), dense(b)),
        (Side::Scalar(a), Side::Datum(b)) => apply(mask, out, &op, scalar(a), datum(b)),
        (Side::Scalar(_), Side::Scalar(_)) => unreachable!("two scalar operands"),
    }
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

/// Every pair of lanes: operate, store, and keep the overflow of the lanes
/// that the mask selects.
#[inline]
#[target_feature(enable = "neon")]
fn apply(
    mask: u64,
    out: &mut [MaybeUninit<i64>; 64],
    op: &impl Fn(int64x2_t, int64x2_t) -> (int64x2_t, uint64x2_t),
    left: impl Fn(usize) -> int64x2_t,
    right: impl Fn(usize) -> int64x2_t,
) -> bool {
    let base = out.as_mut_ptr().cast::<i64>();
    if mask == u64::MAX {
        let mut fits_all = vdupq_n_u64(u64::MAX);
        for pair in 0..32 {
            let (result, fits) = op(left(pair), right(pair));
            fits_all = vandq_u64(fits_all, fits);
            // SAFETY: `pair` is below 32, so the two lanes written end
            // within the array; MaybeUninit<i64> has i64's layout.
            unsafe { vst1q_s64(base.add(pair * 2), result) };
        }
        return vminvq_u32(vreinterpretq_u32_u64(fits_all)) == 0;
    }
    let weights = byte_weights();
    let mut overflow = vdupq_n_u64(0);
    for byte in 0..8 {
        let (first, second) = lane_masks((mask >> (byte * 8)) as u8, weights);
        let (m0, m1) = widen(first);
        let (m2, m3) = widen(second);
        for (offset, lanes) in [m0, m1, m2, m3].into_iter().enumerate() {
            let pair = byte * 4 + offset;
            let (result, fits) = op(left(pair), right(pair));
            overflow = vorrq_u64(overflow, vbicq_u64(lanes, fits));
            // SAFETY: as above.
            unsafe { vst1q_s64(base.add(pair * 2), result) };
        }
    }
    vmaxvq_u32(vreinterpretq_u32_u64(overflow)) != 0
}
