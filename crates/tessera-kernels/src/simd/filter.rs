//! Whole-word comparisons of int4 values with a scalar.

use core::arch::aarch64::{
    int32x4_t, uint32x4_t, vceqq_s32, vcgeq_s32, vcgtq_s32, vcleq_s32, vcltq_s32, vdupq_n_s32,
    vld1q_s32, vmvnq_u32,
};

use super::{load_datums, non_null_lanes, pack_lanes};
use crate::int32::{CompareOp, Side};

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
/// The lanes where `left op right` over two whole-word sides of columns:
/// dense or Datum storage, the two in any combination.
#[inline]
pub fn compare_sides(left: Side<'_>, right: Side<'_>, op: CompareOp) -> u64 {
    const { assert!(cfg!(target_feature = "neon")) }
    // SAFETY: NEON is enabled for this compilation (asserted above).
    unsafe { sides(left, right, op) }
}

/// A loader of four lanes of a column side.
#[inline(always)]
fn side_loader(side: Side<'_>) -> impl Fn(usize) -> int32x4_t + '_ {
    move |group| match side {
        // SAFETY: `group` is below 16, so the four lanes read lie within
        // the 64 of the side.
        Side::Dense(values) => unsafe { vld1q_s32(values.as_ptr().add(group * 4)) },
        // SAFETY: as above, for the loader of Datum words.
        Side::Datum(values) => unsafe { load_datums(values, group) },
        Side::Scalar(_) => unreachable!("a scalar side"),
    }
}

#[target_feature(enable = "neon")]
fn sides(left: Side<'_>, right: Side<'_>, op: CompareOp) -> u64 {
    // One instance per storage pair, so that the loaders fold into the loop.
    match (left, right) {
        (Side::Dense(_), Side::Dense(_))
        | (Side::Dense(_), Side::Datum(_))
        | (Side::Datum(_), Side::Dense(_))
        | (Side::Datum(_), Side::Datum(_)) => pack_pairs(op, side_loader(left), side_loader(right)),
        _ => unreachable!("a scalar side"),
    }
}

#[inline]
#[target_feature(enable = "neon")]
fn pack_pairs(
    op: CompareOp,
    left: impl Fn(usize) -> int32x4_t,
    right: impl Fn(usize) -> int32x4_t,
) -> u64 {
    match op {
        CompareOp::Eq => pack_lanes(|g| vceqq_s32(left(g), right(g))),
        CompareOp::Ne => pack_lanes(|g| vmvnq_u32(vceqq_s32(left(g), right(g)))),
        CompareOp::Lt => pack_lanes(|g| vcltq_s32(left(g), right(g))),
        CompareOp::Le => pack_lanes(|g| vcleq_s32(left(g), right(g))),
        CompareOp::Gt => pack_lanes(|g| vcgtq_s32(left(g), right(g))),
        CompareOp::Ge => pack_lanes(|g| vcgeq_s32(left(g), right(g))),
    }
}

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

/// The bits of the 64 rows where `compare` holds of the loaded lanes.
#[inline]
#[target_feature(enable = "neon")]
fn pack_with(load: impl Fn(usize) -> int32x4_t, compare: impl Fn(int32x4_t) -> uint32x4_t) -> u64 {
    pack_lanes(|group| compare(load(group)))
}
