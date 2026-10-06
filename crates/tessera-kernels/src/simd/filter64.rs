//! Whole-word comparisons of int8 values with a scalar.
//!
//! A word is 32 pairs of lanes; the compare of each pair narrows to two
//! 32-bit lanes, so that four rows form the same four-lane group the int4
//! packing weighs and adds, and the rest of the packing is that of
//! `filter.rs`. A Datum holds an int8 whole, so the Datum block loads like
//! the dense one, without deinterleaving.

use core::arch::aarch64::{
    int64x2_t, vceqq_s64, vcgeq_s64, vcgtq_s64, vcleq_s64, vcltq_s64, vcombine_u32, vdupq_n_s64,
    vld1q_s64, vmovn_u64, vmvnq_u32,
};

use super::{non_null_lanes, pack_lanes};
use crate::int64::Side;
use crate::ops::CompareOp;

/// Rows of a dense block whose value satisfies `value op scalar`, as bits in
/// row order. NULL rows compare like any other and are masked by the caller.
#[inline]
pub fn filter_dense64(values: &[i64; 64], scalar: i64, op: CompareOp) -> u64 {
    const { assert!(cfg!(target_feature = "neon")) }
    // SAFETY: NEON is enabled for this compilation (asserted above), so the
    // target-feature function can run on any CPU this code runs on.
    unsafe { dense(values, scalar, op) }
}

/// Rows of a Datum block whose int8 value satisfies `value op scalar` and
/// whose flag is not NULL, as bits in row order.
#[inline]
pub fn filter_datum64(values: &[u64; 64], isnull: &[bool; 64], scalar: i64, op: CompareOp) -> u64 {
    const { assert!(cfg!(target_feature = "neon")) }
    // SAFETY: as in filter_dense64.
    unsafe { datum(values, isnull, scalar, op) }
}

#[target_feature(enable = "neon")]
fn dense(values: &[i64; 64], scalar: i64, op: CompareOp) -> u64 {
    // SAFETY: `pair` is below 32, so the two lanes read end within the array.
    let load = |pair: usize| unsafe { vld1q_s64(values.as_ptr().add(pair * 2)) };
    pack(op, scalar, load)
}

#[target_feature(enable = "neon")]
fn datum(values: &[u64; 64], isnull: &[bool; 64], scalar: i64, op: CompareOp) -> u64 {
    // SAFETY: `pair` is below 32; a Datum holding an int8 is its bits.
    let load = |pair: usize| unsafe { vld1q_s64(values.as_ptr().cast::<i64>().add(pair * 2)) };
    pack(op, scalar, load) & non_null_lanes(isnull)
}

/// Compare 32 pairs of lanes and pack the results into 64 bits, with the
/// comparison chosen once per word. Each group of four rows is two pairs
/// narrowed into four 32-bit lanes; `Ne` inverts the narrowed group once
/// instead of each pair.
/// The lanes where `left op right` over two whole-word sides of int8
/// columns: dense or Datum storage, the two in any combination; a Datum
/// is the whole word, so both load alike.
#[inline]
pub fn compare_sides64(left: Side<'_>, right: Side<'_>, op: CompareOp) -> u64 {
    const { assert!(cfg!(target_feature = "neon")) }
    // SAFETY: NEON is enabled for this compilation (asserted above).
    unsafe { sides(side_words(left), side_words(right), op) }
}

/// The 64 eight-byte words of a side, dense or Datum alike.
#[inline(always)]
fn side_words(side: Side<'_>) -> &[i64; 64] {
    match side {
        Side::Dense(values) => values,
        // SAFETY: `[u64; 64]` and `[i64; 64]` have the same size and
        // alignment, and every bit pattern is valid for both.
        Side::Datum(values) => unsafe { &*values.as_ptr().cast::<[i64; 64]>() },
        Side::Scalar(_) => unreachable!("a scalar side"),
    }
}

#[target_feature(enable = "neon")]
fn sides(left: &[i64; 64], right: &[i64; 64], op: CompareOp) -> u64 {
    // SAFETY: `pair` is below 32, so the two words read lie within 64.
    let l = |pair: usize| unsafe { vld1q_s64(left.as_ptr().add(pair * 2)) };
    // SAFETY: as above.
    let r = |pair: usize| unsafe { vld1q_s64(right.as_ptr().add(pair * 2)) };
    let group = |compare: &dyn Fn(int64x2_t, int64x2_t) -> core::arch::aarch64::uint64x2_t,
                 group: usize| {
        vcombine_u32(
            vmovn_u64(compare(l(group * 2), r(group * 2))),
            vmovn_u64(compare(l(group * 2 + 1), r(group * 2 + 1))),
        )
    };
    match op {
        CompareOp::Eq => pack_lanes(|g| group(&|a, b| vceqq_s64(a, b), g)),
        CompareOp::Ne => pack_lanes(|g| vmvnq_u32(group(&|a, b| vceqq_s64(a, b), g))),
        CompareOp::Lt => pack_lanes(|g| group(&|a, b| vcltq_s64(a, b), g)),
        CompareOp::Le => pack_lanes(|g| group(&|a, b| vcleq_s64(a, b), g)),
        CompareOp::Gt => pack_lanes(|g| group(&|a, b| vcgtq_s64(a, b), g)),
        CompareOp::Ge => pack_lanes(|g| group(&|a, b| vcgeq_s64(a, b), g)),
    }
}

#[inline]
#[target_feature(enable = "neon")]
fn pack(op: CompareOp, scalar: i64, load: impl Fn(usize) -> int64x2_t) -> u64 {
    let scalar = vdupq_n_s64(scalar);
    let group = |compare: &dyn Fn(int64x2_t) -> core::arch::aarch64::uint64x2_t, group: usize| {
        vcombine_u32(
            vmovn_u64(compare(load(group * 2))),
            vmovn_u64(compare(load(group * 2 + 1))),
        )
    };
    match op {
        CompareOp::Eq => pack_lanes(|g| group(&|v| vceqq_s64(v, scalar), g)),
        CompareOp::Ne => pack_lanes(|g| vmvnq_u32(group(&|v| vceqq_s64(v, scalar), g))),
        CompareOp::Lt => pack_lanes(|g| group(&|v| vcltq_s64(v, scalar), g)),
        CompareOp::Le => pack_lanes(|g| group(&|v| vcleq_s64(v, scalar), g)),
        CompareOp::Gt => pack_lanes(|g| group(&|v| vcgtq_s64(v, scalar), g)),
        CompareOp::Ge => pack_lanes(|g| group(&|v| vcgeq_s64(v, scalar), g)),
    }
}
