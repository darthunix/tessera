//! Vector kernels over the storage of full prepared words.
//!
//! Only AArch64 NEON is implemented, and only where it can be measured: the
//! module is empty on other targets and under Miri, and callers fall back to
//! the row paths. Instructions always drop with vectors, so a kernel here is
//! kept on its cycle gain (see the benchmark guide). This is the one module
//! of the crate allowed to use `unsafe`, for vector loads from borrowed
//! arrays; every load states why it stays in bounds.
//!
//! Intrinsics are callable without `unsafe` only from functions carrying the
//! `neon` feature; closures inherit it from the function that defines them.
//! Public entry points are plain functions that assert the feature at compile
//! time and make the one `unsafe` call into the feature-carrying code.
#![cfg(all(target_arch = "aarch64", not(miri)))]
#![allow(unsafe_code)]

mod aggregate;
mod filter;

use core::arch::aarch64::{
    int32x4_t, uint8x8_t, uint32x4_t, vaddv_u8, vandq_u8, vceqzq_u8, vdup_n_u8, vget_high_s16,
    vget_high_u8, vget_low_s16, vget_low_u8, vld1_u8, vld1q_u8, vld1q_u64, vmovl_s8, vmovl_s16,
    vreinterpret_s8_u8, vreinterpretq_s32_u64, vreinterpretq_u32_s32, vtst_u8, vuzp1q_s32,
};

pub use aggregate::{
    count_datum, max_datum, max_dense, min_datum, min_dense, sum_datum, sum_dense,
};
pub use filter::{filter_datum, filter_dense};

/// Bit weights of the four lanes of each group in a 16-row quarter.
const LANE_WEIGHTS: [[u32; 4]; 4] = [
    [1, 1 << 1, 1 << 2, 1 << 3],
    [1 << 4, 1 << 5, 1 << 6, 1 << 7],
    [1 << 8, 1 << 9, 1 << 10, 1 << 11],
    [1 << 12, 1 << 13, 1 << 14, 1 << 15],
];
/// Bit weights of the bytes of two 8-row halves.
const BYTE_WEIGHTS: [u8; 16] = [1, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128];

/// Rows of a Datum block whose flag is false, as bits in row order.
#[inline]
pub fn non_null_bits(isnull: &[bool; 64]) -> u64 {
    const { assert!(cfg!(target_feature = "neon")) }
    // SAFETY: NEON is enabled for this compilation (asserted above), so the
    // target-feature function can run on any CPU this code runs on.
    unsafe { non_null_lanes(isnull) }
}

#[inline]
#[target_feature(enable = "neon")]
fn non_null_lanes(isnull: &[bool; 64]) -> u64 {
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

/// The bit weights of eight rows, one per byte lane.
#[inline]
#[target_feature(enable = "neon")]
fn byte_weights() -> uint8x8_t {
    // SAFETY: eight weights.
    unsafe { vld1_u8(BYTE_WEIGHTS.as_ptr()) }
}

/// Eight mask bits as two groups of four lane masks: a set bit becomes an
/// all-ones lane. The byte is broadcast, tested against each lane's weight
/// and sign-extended twice.
#[inline]
#[target_feature(enable = "neon")]
fn lane_masks(bits: u8, weights: uint8x8_t) -> (uint32x4_t, uint32x4_t) {
    let bytes = vreinterpret_s8_u8(vtst_u8(vdup_n_u8(bits), weights));
    let halves = vmovl_s8(bytes);
    (
        vreinterpretq_u32_s32(vmovl_s16(vget_low_s16(halves))),
        vreinterpretq_u32_s32(vmovl_s16(vget_high_s16(halves))),
    )
}

/// Four int4 values from four Datums: the low half of each, kept by uzp1.
///
/// # Safety
///
/// `group` must be below 16 so that the four Datums read end within the array.
#[inline]
#[target_feature(enable = "neon")]
unsafe fn load_datums(values: &[u64; 64], group: usize) -> int32x4_t {
    // SAFETY: the caller keeps `group` below 16.
    let (first, second) = unsafe {
        let start = values.as_ptr().add(group * 4);
        (vld1q_u64(start), vld1q_u64(start.add(2)))
    };
    vuzp1q_s32(vreinterpretq_s32_u64(first), vreinterpretq_s32_u64(second))
}
