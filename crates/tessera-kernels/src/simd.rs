//! Vector kernels over the storage of full prepared words.
//!
//! Only AArch64 NEON is implemented, and only where it can be measured: the
//! module is empty on other targets and under Miri, and callers fall back to
//! the row paths. Instructions always drop with vectors, so a kernel here is
//! kept on its cycle gain (see the benchmark guide). This is the one module
//! of the crate allowed to use `unsafe`, for vector loads from borrowed
//! arrays; every load states why it stays in bounds.
#![cfg(all(target_arch = "aarch64", not(miri)))]
#![allow(unsafe_code)]

use core::arch::aarch64::{
    int32x4_t, uint32x4_t, vaddv_u8, vaddvq_u32, vandq_u8, vandq_u32, vceqq_s32, vceqzq_u8,
    vcgeq_s32, vcgtq_s32, vcleq_s32, vcltq_s32, vdupq_n_s32, vdupq_n_u32, vget_high_u8,
    vget_low_u8, vld1q_s32, vld1q_u8, vld1q_u32, vld1q_u64, vmvnq_u32, vorrq_u32,
    vreinterpretq_s32_u64, vuzp1q_s32,
};

use crate::int32::CompareOp;

/// Bit weights of the four lanes of each group in a 16-row quarter.
const LANE_WEIGHTS: [[u32; 4]; 4] = [
    [1, 1 << 1, 1 << 2, 1 << 3],
    [1 << 4, 1 << 5, 1 << 6, 1 << 7],
    [1 << 8, 1 << 9, 1 << 10, 1 << 11],
    [1 << 12, 1 << 13, 1 << 14, 1 << 15],
];
/// Bit weights of the bytes of two 8-row halves.
const BYTE_WEIGHTS: [u8; 16] = [1, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128];

/// Rows of a dense block whose value satisfies `value op scalar`, as bits in
/// row order. NULL rows compare like any other and are masked by the caller.
pub fn filter_dense(values: &[i32; 64], scalar: i32, op: CompareOp) -> u64 {
    const { assert!(cfg!(target_feature = "neon")) }
    // SAFETY: NEON is enabled for this compilation (asserted above), so the
    // target-feature function can run on any CPU this code runs on.
    unsafe { dense(values, scalar, op) }
}

/// Rows of a Datum block whose int4 value satisfies `value op scalar` and
/// whose flag is not NULL, as bits in row order.
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
    // A Datum's low 32 bits are its int4 (DatumGetInt32); uzp1 keeps the low
    // half of each of four Datums.
    let load = |group: usize| {
        // SAFETY: `group` is below 16, so the four Datums read end within the array.
        let (first, second) = unsafe {
            let start = values.as_ptr().add(group * 4);
            (vld1q_u64(start), vld1q_u64(start.add(2)))
        };
        vuzp1q_s32(vreinterpretq_s32_u64(first), vreinterpretq_s32_u64(second))
    };
    pack(op, scalar, load) & non_null_bits(isnull)
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

/// Rows whose flag is false, as bits in row order.
#[inline]
#[target_feature(enable = "neon")]
fn non_null_bits(isnull: &[bool; 64]) -> u64 {
    // SAFETY: sixteen weights.
    let weights = unsafe { vld1q_u8(BYTE_WEIGHTS.as_ptr()) };
    let mut bits = 0;
    for quarter in 0..4 {
        // SAFETY: a bool is one byte holding 0 or 1, and the sixteen bytes
        // read end within the array.
        let flags = unsafe { vld1q_u8(isnull.as_ptr().cast::<u8>().add(quarter * 16)) };
        let present = vandq_u8(vceqzq_u8(flags), weights);
        let low = u64::from(vaddv_u8(vget_low_u8(present)));
        let high = u64::from(vaddv_u8(vget_high_u8(present)));
        bits |= (low | high << 8) << (quarter * 16);
    }
    bits
}
