//! Whole-word comparisons of int4 values with a scalar.

use core::arch::aarch64::{
    int32x4_t, uint32x4_t, vaddvq_u32, vandq_u32, vceqq_s32, vcgeq_s32, vcgtq_s32, vcleq_s32,
    vcltq_s32, vdupq_n_s32, vdupq_n_u32, vld1q_s32, vld1q_u32, vmvnq_u32, vorrq_u32,
};

use super::{LANE_WEIGHTS, load_datums, non_null_lanes};
use crate::int32::CompareOp;

/// Rows of a dense block whose value satisfies `value op scalar`, as bits in
/// row order. NULL rows compare like any other and are masked by the caller.
#[inline]
pub fn filter_dense(values: &[i32; 64], scalar: i32, op: CompareOp) -> u64 {
    const { assert!(cfg!(target_feature = "neon")) }
    // SAFETY: NEON is enabled for this compilation (asserted above), so the
    // target-feature function can run on any CPU this code runs on.
    unsafe { dense(values, scalar, op) }
}

/// Rows of a Datum block whose int4 value satisfies `value op scalar` and
/// whose flag is not NULL, as bits in row order.
#[inline]
pub fn filter_datum(values: &[u64; 64], isnull: &[bool; 64], scalar: i32, op: CompareOp) -> u64 {
    const { assert!(cfg!(target_feature = "neon")) }
    // SAFETY: as in filter_dense.
    unsafe { datum(values, isnull, scalar, op) }
}

// Intrinsics are callable without `unsafe` only from functions carrying the
// feature; closures inherit it from the function that defines them.

#[target_feature(enable = "neon")]
fn dense(values: &[i32; 64], scalar: i32, op: CompareOp) -> u64 {
    // SAFETY: `group` is below 16, so the four lanes read end within the array.
    let load = |group: usize| unsafe { vld1q_s32(values.as_ptr().add(group * 4)) };
    pack(op, scalar, load)
}

#[target_feature(enable = "neon")]
fn datum(values: &[u64; 64], isnull: &[bool; 64], scalar: i32, op: CompareOp) -> u64 {
    // SAFETY: `group` is below 16.
    let load = |group: usize| unsafe { load_datums(values, group) };
    pack(op, scalar, load) & non_null_lanes(isnull)
}

/// Compare 16 groups of four lanes and pack the results into 64 bits, with
/// the comparison chosen once per word.
#[inline]
#[target_feature(enable = "neon")]
fn pack(op: CompareOp, scalar: i32, load: impl Fn(usize) -> int32x4_t) -> u64 {
    let scalar = vdupq_n_s32(scalar);
    match op {
        CompareOp::Eq => pack_with(load, |v| vceqq_s32(v, scalar)),
        CompareOp::Ne => pack_with(load, |v| vmvnq_u32(vceqq_s32(v, scalar))),
        CompareOp::Lt => pack_with(load, |v| vcltq_s32(v, scalar)),
        CompareOp::Le => pack_with(load, |v| vcleq_s32(v, scalar)),
        CompareOp::Gt => pack_with(load, |v| vcgtq_s32(v, scalar)),
        CompareOp::Ge => pack_with(load, |v| vcgeq_s32(v, scalar)),
    }
}

/// A passing lane is all ones. Weighting each lane by its bit and adding the
/// four groups of a quarter gives the quarter's 16 bits without a per-row
/// shift; four quarters make the word.
#[inline]
#[target_feature(enable = "neon")]
fn pack_with(load: impl Fn(usize) -> int32x4_t, compare: impl Fn(int32x4_t) -> uint32x4_t) -> u64 {
    // SAFETY: every weight row has exactly four lanes.
    let weights = LANE_WEIGHTS.map(|row| unsafe { vld1q_u32(row.as_ptr()) });
    let mut bits = 0;
    for quarter in 0..4 {
        let mut passing = vdupq_n_u32(0);
        for (group, weight) in weights.iter().enumerate() {
            let lanes = compare(load(quarter * 4 + group));
            passing = vorrq_u32(passing, vandq_u32(lanes, *weight));
        }
        bits |= u64::from(vaddvq_u32(passing)) << (quarter * 16);
    }
    bits
}
