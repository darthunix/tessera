//! Whole-word widening of int4 values into int8 lanes.
//!
//! Every lane of the word is widened and stored, NULL and unselected
//! lanes included: their placeholders are what the output contract allows,
//! and the caller's mask says which lanes hold a value.

use core::arch::aarch64::{
    int32x4_t, vget_low_s32, vld1q_s32, vmovl_high_s32, vmovl_s32, vst1q_s64,
};
use std::mem::MaybeUninit;

use super::load_datums;

/// The int4 values of a dense block widened into `out`.
#[inline]
pub fn widen_dense(values: &[i32; 64], out: &mut [MaybeUninit<i64>; 64]) {
    const { assert!(cfg!(target_feature = "neon")) }
    // SAFETY: NEON is enabled for this compilation (asserted above).
    unsafe { widen_dense_lanes(values, out) }
}

/// The int4 values of a Datum block widened into `out`.
#[inline]
pub fn widen_datum(values: &[u64; 64], out: &mut [MaybeUninit<i64>; 64]) {
    const { assert!(cfg!(target_feature = "neon")) }
    // SAFETY: as in widen_dense.
    unsafe { widen_datum_lanes(values, out) }
}

#[target_feature(enable = "neon")]
fn widen_dense_lanes(values: &[i32; 64], out: &mut [MaybeUninit<i64>; 64]) {
    // SAFETY: `group` is below 16, so the four lanes read end within the array.
    let load = |group: usize| unsafe { vld1q_s32(values.as_ptr().add(group * 4)) };
    store(out, load);
}

#[target_feature(enable = "neon")]
fn widen_datum_lanes(values: &[u64; 64], out: &mut [MaybeUninit<i64>; 64]) {
    // SAFETY: `group` is below 16.
    let load = |group: usize| unsafe { load_datums(values, group) };
    store(out, load);
}

/// Every group of four int4 lanes sign-extended into two pairs of int8
/// lanes, stored in place.
#[inline]
#[target_feature(enable = "neon")]
fn store(out: &mut [MaybeUninit<i64>; 64], load: impl Fn(usize) -> int32x4_t) {
    let base = out.as_mut_ptr().cast::<i64>();
    for group in 0..16 {
        let lanes = load(group);
        // SAFETY: `group` is below 16, so the four lanes written end within
        // the array; MaybeUninit<i64> has i64's layout.
        unsafe {
            vst1q_s64(base.add(group * 4), vmovl_s32(vget_low_s32(lanes)));
            vst1q_s64(base.add(group * 4 + 2), vmovl_high_s32(lanes));
        }
    }
}
