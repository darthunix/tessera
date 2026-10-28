//! Whole-word int8 addition and subtraction with overflow detection,
//! written once over the primitives of [`Vectors`].
//!
//! Every lane is computed modulo 2^64 and stored; overflow is detected per
//! lane, kept only for the lanes of `mask`, and reported once per word.
//! NULL and unselected lanes thus get initialized placeholders and never
//! raise. Neither set multiplies 64-bit lanes, so the multiplications and
//! divisions of a word stay in the caller's lane loop. A word is `64 /
//! LANES64` vectors, loaded alike from dense storage and from Datums,
//! which hold an int8 whole.

// Under Miri no set runs, so the tokens have no values, and the code that
// the functions compiled for a set run on theirs cannot be reached.
#![cfg_attr(miri, allow(unreachable_code, unused_variables))]

use std::mem::MaybeUninit;

use super::{Neon, Simd, Vectors};
use crate::int64::Side;

/// `lhs + rhs` into `out`; true when a masked lane overflowed.
#[inline]
pub fn add64(
    neon: Neon,
    lhs: Side<'_>,
    rhs: Side<'_>,
    mask: u64,
    out: &mut [MaybeUninit<i64>; 64],
) -> bool {
    // SAFETY: the token proves that this processor runs NEON, the vector
    // set of the target where it has values, which `add_lanes` is compiled
    // for.
    unsafe { add_lanes(neon.simd(), lhs, rhs, mask, out) }
}

/// `lhs - rhs` into `out`; true when a masked lane overflowed.
#[inline]
pub fn sub64(
    neon: Neon,
    lhs: Side<'_>,
    rhs: Side<'_>,
    mask: u64,
    out: &mut [MaybeUninit<i64>; 64],
) -> bool {
    // SAFETY: as in add64, for `sub_lanes`.
    unsafe { sub_lanes(neon.simd(), lhs, rhs, mask, out) }
}

// The functions compiled for the vector set of the target, into which the
// kernels below are inlined; the kernels carry its feature and are
// `#[inline]`, as in `arith.rs`. The operation returns the wrapped result
// and the lanes that fit, all ones where no overflow occurred.

#[cfg_attr(
    all(target_arch = "aarch64", not(miri)),
    target_feature(enable = "neon")
)]
#[cfg_attr(
    all(target_arch = "x86_64", not(miri)),
    target_feature(enable = "avx2")
)]
unsafe fn add_lanes(
    simd: Simd,
    lhs: Side<'_>,
    rhs: Side<'_>,
    mask: u64,
    out: &mut [MaybeUninit<i64>; 64],
) -> bool {
    dispatch(simd, lhs, rhs, mask, out, |a, b| {
        let sum = simd.wrapping_add64(a, b);
        (sum, simd.add_fits64(a, b, sum))
    })
}

#[cfg_attr(
    all(target_arch = "aarch64", not(miri)),
    target_feature(enable = "neon")
)]
#[cfg_attr(
    all(target_arch = "x86_64", not(miri)),
    target_feature(enable = "avx2")
)]
unsafe fn sub_lanes(
    simd: Simd,
    lhs: Side<'_>,
    rhs: Side<'_>,
    mask: u64,
    out: &mut [MaybeUninit<i64>; 64],
) -> bool {
    dispatch(simd, lhs, rhs, mask, out, |a, b| {
        let difference = simd.wrapping_sub64(a, b);
        (difference, simd.sub_fits64(a, b, difference))
    })
}

/// A loader of the vectors of a dense block.
#[inline]
#[cfg_attr(
    all(target_arch = "aarch64", not(miri)),
    target_feature(enable = "neon")
)]
#[cfg_attr(
    all(target_arch = "x86_64", not(miri)),
    target_feature(enable = "avx2")
)]
fn dense<V: Vectors>(v: V, values: &[i64; 64]) -> impl Fn(usize) -> V::I64 + '_ {
    // SAFETY: `apply` asks for the vectors of a word only.
    move |index| unsafe { v.load64(values, index) }
}

/// A loader of the vectors of a Datum block: a Datum holding an int8 is
/// its bits.
#[inline]
#[cfg_attr(
    all(target_arch = "aarch64", not(miri)),
    target_feature(enable = "neon")
)]
#[cfg_attr(
    all(target_arch = "x86_64", not(miri)),
    target_feature(enable = "avx2")
)]
fn datum<V: Vectors>(v: V, values: &[u64; 64]) -> impl Fn(usize) -> V::I64 + '_ {
    // SAFETY: as in `dense`.
    move |index| unsafe { v.load_datums64(values, index) }
}

#[inline]
#[cfg_attr(
    all(target_arch = "aarch64", not(miri)),
    target_feature(enable = "neon")
)]
#[cfg_attr(
    all(target_arch = "x86_64", not(miri)),
    target_feature(enable = "avx2")
)]
fn scalar<V: Vectors>(v: V, value: i64) -> impl Fn(usize) -> V::I64 {
    let lanes = v.splat64(value);
    move |_| lanes
}

/// Choose the loaders for the operand kinds once per word.
#[inline]
#[cfg_attr(
    all(target_arch = "aarch64", not(miri)),
    target_feature(enable = "neon")
)]
#[cfg_attr(
    all(target_arch = "x86_64", not(miri)),
    target_feature(enable = "avx2")
)]
fn dispatch<V: Vectors>(
    v: V,
    lhs: Side<'_>,
    rhs: Side<'_>,
    mask: u64,
    out: &mut [MaybeUninit<i64>; 64],
    op: impl Fn(V::I64, V::I64) -> (V::I64, V::M64),
) -> bool {
    match (lhs, rhs) {
        (Side::Dense(a), Side::Dense(b)) => apply(v, mask, out, &op, dense(v, a), dense(v, b)),
        (Side::Dense(a), Side::Datum(b)) => apply(v, mask, out, &op, dense(v, a), datum(v, b)),
        (Side::Dense(a), Side::Scalar(b)) => apply(v, mask, out, &op, dense(v, a), scalar(v, b)),
        (Side::Datum(a), Side::Dense(b)) => apply(v, mask, out, &op, datum(v, a), dense(v, b)),
        (Side::Datum(a), Side::Datum(b)) => apply(v, mask, out, &op, datum(v, a), datum(v, b)),
        (Side::Datum(a), Side::Scalar(b)) => apply(v, mask, out, &op, datum(v, a), scalar(v, b)),
        (Side::Scalar(a), Side::Dense(b)) => apply(v, mask, out, &op, scalar(v, a), dense(v, b)),
        (Side::Scalar(a), Side::Datum(b)) => apply(v, mask, out, &op, scalar(v, a), datum(v, b)),
        (Side::Scalar(_), Side::Scalar(_)) => unreachable!("two scalar operands"),
    }
}

/// Every vector: operate, store, and keep the overflow of the lanes that
/// the mask selects. A full word keeps the lanes that fit, and overflows
/// unless all of them do.
#[inline]
#[cfg_attr(
    all(target_arch = "aarch64", not(miri)),
    target_feature(enable = "neon")
)]
#[cfg_attr(
    all(target_arch = "x86_64", not(miri)),
    target_feature(enable = "avx2")
)]
fn apply<V: Vectors>(
    v: V,
    mask: u64,
    out: &mut [MaybeUninit<i64>; 64],
    op: &impl Fn(V::I64, V::I64) -> (V::I64, V::M64),
    left: impl Fn(usize) -> V::I64,
    right: impl Fn(usize) -> V::I64,
) -> bool {
    let base = out.as_mut_ptr().cast::<i64>();
    if mask == u64::MAX {
        let mut fits_all = v.every64();
        for index in 0..64 / V::LANES64 {
            let (result, fits) = op(left(index), right(index));
            fits_all = v.and64(fits_all, fits);
            // SAFETY: `index` is below `64 / LANES64`, so the values written
            // end within the word; MaybeUninit<i64> has i64's layout.
            unsafe { v.store64(base.add(index * V::LANES64), result) };
        }
        return !v.all64(fits_all);
    }
    let mut overflow = v.none64();
    for byte in 0..8 {
        let masks = v.lane_masks64((mask >> (byte * 8)) as u8);
        for (index, lanes) in (byte * (8 / V::LANES64)..).zip(masks) {
            let (result, fits) = op(left(index), right(index));
            overflow = v.or64(overflow, v.and_not64(lanes, fits));
            // SAFETY: as above.
            unsafe { v.store64(base.add(index * V::LANES64), result) };
        }
    }
    v.any64(overflow)
}
