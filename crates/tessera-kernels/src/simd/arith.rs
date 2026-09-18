//! Whole-word int4 addition, subtraction and multiplication with overflow
//! detection.
//!
//! Every lane is computed modulo 2^32 and stored; overflow is detected per
//! lane (saturating against wrapping results for `+` and `-`, the widening
//! product against the sign-extended narrow one for `*`), kept only for the
//! lanes of `mask`, and reported once per word. NULL and unselected lanes
//! thus get initialized placeholders and never raise. Division has no
//! vector instruction and stays in the caller's lane loop.

use core::arch::aarch64::{
    int32x4_t, uint32x4_t, vaddq_s32, vandq_u32, vceqq_s32, vceqq_s64, vcombine_u32, vdupq_n_s32,
    vdupq_n_u32, vget_low_s32, vld1q_s32, vmaxvq_u32, vmovl_high_s32, vmovl_s32, vmovn_u64,
    vmull_high_s32, vmull_s32, vmulq_s32, vmvnq_u32, vorrq_u32, vqaddq_s32, vqsubq_s32, vst1q_s32,
    vsubq_s32,
};
use std::mem::MaybeUninit;

use super::{byte_weights, lane_masks, load_datums};
use crate::int32::Side;

/// `lhs + rhs` into `out`; true when a masked lane overflowed.
#[inline]
pub fn add(lhs: Side<'_>, rhs: Side<'_>, mask: u64, out: &mut [MaybeUninit<i32>; 64]) -> bool {
    const { assert!(cfg!(target_feature = "neon")) }
    // SAFETY: NEON is enabled for this compilation (asserted above).
    unsafe { add_lanes(lhs, rhs, mask, out) }
}

/// `lhs - rhs` into `out`; true when a masked lane overflowed.
#[inline]
pub fn sub(lhs: Side<'_>, rhs: Side<'_>, mask: u64, out: &mut [MaybeUninit<i32>; 64]) -> bool {
    const { assert!(cfg!(target_feature = "neon")) }
    // SAFETY: as in add.
    unsafe { sub_lanes(lhs, rhs, mask, out) }
}

/// `lhs * rhs` into `out`; true when a masked lane overflowed.
#[inline]
pub fn mul(lhs: Side<'_>, rhs: Side<'_>, mask: u64, out: &mut [MaybeUninit<i32>; 64]) -> bool {
    const { assert!(cfg!(target_feature = "neon")) }
    // SAFETY: as in add.
    unsafe { mul_lanes(lhs, rhs, mask, out) }
}

#[target_feature(enable = "neon")]
fn add_lanes(lhs: Side<'_>, rhs: Side<'_>, mask: u64, out: &mut [MaybeUninit<i32>; 64]) -> bool {
    dispatch(lhs, rhs, mask, out, |a, b| {
        let wrapped = vaddq_s32(a, b);
        (wrapped, vmvnq_u32(vceqq_s32(wrapped, vqaddq_s32(a, b))))
    })
}

#[target_feature(enable = "neon")]
fn sub_lanes(lhs: Side<'_>, rhs: Side<'_>, mask: u64, out: &mut [MaybeUninit<i32>; 64]) -> bool {
    dispatch(lhs, rhs, mask, out, |a, b| {
        let wrapped = vsubq_s32(a, b);
        (wrapped, vmvnq_u32(vceqq_s32(wrapped, vqsubq_s32(a, b))))
    })
}

#[target_feature(enable = "neon")]
fn mul_lanes(lhs: Side<'_>, rhs: Side<'_>, mask: u64, out: &mut [MaybeUninit<i32>; 64]) -> bool {
    dispatch(lhs, rhs, mask, out, |a, b| {
        let wrapped = vmulq_s32(a, b);
        // The product fits when its 64-bit form equals the sign-extended
        // 32-bit form.
        let low = vceqq_s64(
            vmull_s32(vget_low_s32(a), vget_low_s32(b)),
            vmovl_s32(vget_low_s32(wrapped)),
        );
        let high = vceqq_s64(vmull_high_s32(a, b), vmovl_high_s32(wrapped));
        let fits = vcombine_u32(vmovn_u64(low), vmovn_u64(high));
        (wrapped, vmvnq_u32(fits))
    })
}

/// Choose the loaders for the operand kinds once per word.
#[inline]
#[target_feature(enable = "neon")]
fn dispatch(
    lhs: Side<'_>,
    rhs: Side<'_>,
    mask: u64,
    out: &mut [MaybeUninit<i32>; 64],
    op: impl Fn(int32x4_t, int32x4_t) -> (int32x4_t, uint32x4_t),
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

#[inline]
#[target_feature(enable = "neon")]
fn dense(values: &[i32; 64]) -> impl Fn(usize) -> int32x4_t + '_ {
    // SAFETY: `group` is below 16, so the four lanes read end within the array.
    move |group| unsafe { vld1q_s32(values.as_ptr().add(group * 4)) }
}

#[inline]
#[target_feature(enable = "neon")]
fn datum(values: &[u64; 64]) -> impl Fn(usize) -> int32x4_t + '_ {
    // SAFETY: `group` is below 16.
    move |group| unsafe { load_datums(values, group) }
}

#[inline]
#[target_feature(enable = "neon")]
fn scalar(value: i32) -> impl Fn(usize) -> int32x4_t {
    let lanes = vdupq_n_s32(value);
    move |_| lanes
}

/// Every group of four lanes: operate, store, and keep the overflow lanes
/// that the mask selects.
#[inline]
#[target_feature(enable = "neon")]
fn apply(
    mask: u64,
    out: &mut [MaybeUninit<i32>; 64],
    op: &impl Fn(int32x4_t, int32x4_t) -> (int32x4_t, uint32x4_t),
    left: impl Fn(usize) -> int32x4_t,
    right: impl Fn(usize) -> int32x4_t,
) -> bool {
    let mut overflow = vdupq_n_u32(0);
    let base = out.as_mut_ptr().cast::<i32>();
    if mask == u64::MAX {
        for group in 0..16 {
            let (result, over) = op(left(group), right(group));
            overflow = vorrq_u32(overflow, over);
            // SAFETY: `group` is below 16, so the four lanes written end
            // within the array; MaybeUninit<i32> has i32's layout.
            unsafe { vst1q_s32(base.add(group * 4), result) };
        }
    } else {
        let weights = byte_weights();
        for byte in 0..8 {
            let (first, second) = lane_masks((mask >> (byte * 8)) as u8, weights);
            for (group, lanes) in [(byte * 2, first), (byte * 2 + 1, second)] {
                let (result, over) = op(left(group), right(group));
                overflow = vorrq_u32(overflow, vandq_u32(over, lanes));
                // SAFETY: as above.
                unsafe { vst1q_s32(base.add(group * 4), result) };
            }
        }
    }
    vmaxvq_u32(overflow) != 0
}
