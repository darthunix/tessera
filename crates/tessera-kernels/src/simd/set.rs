//! Whole-word membership of integer values in a few keys: every lane is
//! compared with every key and the equalities or-ed, so that a word costs
//! the same whatever its values. The lanes of a pass stay in registers
//! while the keys go by, each key broadcast once a pass; a pass is packed
//! into bits as in `filter.rs`, each group of four lanes weighted by its
//! bits and a quarter's groups added.

use core::arch::aarch64::{
    int32x4_t, int64x2_t, uint32x4_t, vaddvq_u32, vandq_u32, vceqq_s32, vceqq_s64, vcombine_u32,
    vdupq_n_s32, vdupq_n_s64, vdupq_n_u32, vdupq_n_u64, vld1q_s64, vld1q_u32, vmovn_u64, vorrq_u32,
    vorrq_u64,
};
use core::array;

use super::{LANE_WEIGHTS, datum, dense};

/// Rows of a dense block whose value is one of `keys`, as bits in row
/// order. NULL rows compare like any other and are masked by the caller.
#[inline]
pub fn set_dense(values: &[i32; 64], keys: &[i32]) -> u64 {
    const { assert!(cfg!(target_feature = "neon")) }
    // SAFETY: NEON is enabled for this compilation (asserted above), so the
    // target-feature function can run on any CPU this code runs on.
    unsafe { set32(dense(values), keys) }
}

/// Rows of a Datum block whose int4 value is one of `keys`, NULL rows
/// included as above.
#[inline]
pub fn set_datum(values: &[u64; 64], keys: &[i32]) -> u64 {
    const { assert!(cfg!(target_feature = "neon")) }
    // SAFETY: as in set_dense.
    unsafe { set32(datum(values), keys) }
}

/// Rows of a dense block of int8 values that are one of `keys`, NULL rows
/// included as above.
#[inline]
pub fn set_dense64(values: &[i64; 64], keys: &[i64]) -> u64 {
    const { assert!(cfg!(target_feature = "neon")) }
    // SAFETY: as in set_dense.
    unsafe { set64(values, keys) }
}

/// Rows of a Datum block whose int8 value is one of `keys`, NULL rows
/// included as above.
#[inline]
pub fn set_datum64(values: &[u64; 64], keys: &[i64]) -> u64 {
    const { assert!(cfg!(target_feature = "neon")) }
    // SAFETY: `[u64; 64]` and `[i64; 64]` have the same size and alignment,
    // and every bit pattern is valid for both: a Datum holding an int8 is
    // its bits.
    let values = unsafe { &*values.as_ptr().cast::<[i64; 64]>() };
    // SAFETY: as in set_dense.
    unsafe { set64(values, keys) }
}

/// Two passes of eight groups of four int4 lanes.
#[inline]
#[target_feature(enable = "neon")]
fn set32(load: impl Fn(usize) -> int32x4_t, keys: &[i32]) -> u64 {
    let mut bits = 0;
    for half in 0..2 {
        let values: [int32x4_t; 8] = array::from_fn(|group| load(half * 8 + group));
        let mut equal = [vdupq_n_u32(0); 8];
        for &key in keys {
            let key = vdupq_n_s32(key);
            for (equal, values) in equal.iter_mut().zip(values) {
                *equal = vorrq_u32(*equal, vceqq_s32(values, key));
            }
        }
        bits |= quarter_bits(&equal[..4]) << (half * 32);
        bits |= quarter_bits(&equal[4..]) << (half * 32 + 16);
    }
    bits
}

/// Four passes of four groups, each two pairs of int8 lanes narrowed into
/// four 32-bit lanes after the keys.
#[inline]
#[target_feature(enable = "neon")]
fn set64(values: &[i64; 64], keys: &[i64]) -> u64 {
    let mut bits = 0;
    for quarter in 0..4 {
        // SAFETY: `quarter` is below 4, so the sixteen lanes read end within
        // the array.
        let pairs: [int64x2_t; 8] = array::from_fn(|pair| unsafe {
            vld1q_s64(values.as_ptr().add(quarter * 16 + pair * 2))
        });
        let mut equal = [vdupq_n_u64(0); 8];
        for &key in keys {
            let key = vdupq_n_s64(key);
            for (equal, pairs) in equal.iter_mut().zip(pairs) {
                *equal = vorrq_u64(*equal, vceqq_s64(pairs, key));
            }
        }
        let groups: [uint32x4_t; 4] = array::from_fn(|group| {
            vcombine_u32(vmovn_u64(equal[group * 2]), vmovn_u64(equal[group * 2 + 1]))
        });
        bits |= quarter_bits(&groups) << (quarter * 16);
    }
    bits
}

/// The 16 bits of four groups of lane masks, in row order.
#[inline]
#[target_feature(enable = "neon")]
fn quarter_bits(groups: &[uint32x4_t]) -> u64 {
    let mut passing = vdupq_n_u32(0);
    for (group, weights) in groups.iter().zip(LANE_WEIGHTS) {
        // SAFETY: every weight row has exactly four lanes.
        let weights = unsafe { vld1q_u32(weights.as_ptr()) };
        passing = vorrq_u32(passing, vandq_u32(*group, weights));
    }
    u64::from(vaddvq_u32(passing))
}
