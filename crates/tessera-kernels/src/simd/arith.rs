//! Whole-word int4 addition, subtraction and multiplication with overflow
//! detection, and division by a prepared scalar divisor, written once over
//! the primitives of [`Vectors`].
//!
//! Every lane is computed modulo 2^32 and stored; overflow is detected per
//! lane, kept only for the lanes of `mask`, and reported once per word.
//! NULL and unselected lanes thus get initialized placeholders and never
//! raise. Division by a scalar is a multiplication by the divisor's
//! prepared reciprocal ([`Divisor`]), which no lane can fail, so it takes
//! no mask; division by a column stays in the caller's lane loop, since
//! neither set has an integer division.

// Under Miri no set runs, so the tokens have no values, and the code that
// the functions compiled for a set run on theirs cannot be reached.
#![cfg_attr(miri, allow(unreachable_code, unused_variables))]

use std::mem::MaybeUninit;

use super::{Simd, Vectors};
use crate::int32::{Divisor, Side};

/// `lhs + rhs` into `out`; true when a masked lane overflowed.
#[inline]
pub fn add(
    simd: Simd,
    lhs: Side<'_>,
    rhs: Side<'_>,
    mask: u64,
    out: &mut [MaybeUninit<i32>; 64],
) -> bool {
    // SAFETY: the token proves that this processor runs the vector set of
    // the target, which `add_lanes` is compiled for.
    unsafe { add_lanes(simd, lhs, rhs, mask, out) }
}

/// `lhs - rhs` into `out`; true when a masked lane overflowed.
#[inline]
pub fn sub(
    simd: Simd,
    lhs: Side<'_>,
    rhs: Side<'_>,
    mask: u64,
    out: &mut [MaybeUninit<i32>; 64],
) -> bool {
    // SAFETY: as in add, for `sub_lanes`.
    unsafe { sub_lanes(simd, lhs, rhs, mask, out) }
}

/// `lhs * rhs` into `out`; true when a masked lane overflowed.
#[inline]
pub fn mul(
    simd: Simd,
    lhs: Side<'_>,
    rhs: Side<'_>,
    mask: u64,
    out: &mut [MaybeUninit<i32>; 64],
) -> bool {
    // SAFETY: as in add, for `mul_lanes`.
    unsafe { mul_lanes(simd, lhs, rhs, mask, out) }
}

/// `lhs / divisor` into every lane of `out`; no lane can fail.
#[inline]
pub fn div(simd: Simd, lhs: Side<'_>, divisor: &Divisor, out: &mut [MaybeUninit<i32>; 64]) {
    // SAFETY: as in add, for `div_lanes`.
    unsafe { div_lanes(simd, lhs, divisor, out) }
}

/// `lhs % divisor` into every lane of `out`; no lane can fail.
#[inline]
pub fn rem(simd: Simd, lhs: Side<'_>, divisor: &Divisor, out: &mut [MaybeUninit<i32>; 64]) {
    // SAFETY: as in add, for `rem_lanes`.
    unsafe { rem_lanes(simd, lhs, divisor, out) }
}

// The functions compiled for the vector set of the target, into which the
// kernels below are inlined, as in `filter.rs`. The kernels carry the
// feature of the target's set and are `#[inline]`, as NEON's helpers of
// this code were: `#[inline(always)]` merged them into one body before
// the compiler had simplified each, and the NEON code changed, a word's
// stores no longer paired and its masked loop no longer unrolled.

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
    out: &mut [MaybeUninit<i32>; 64],
) -> bool {
    dispatch(simd, lhs, rhs, mask, out, |a, b| {
        let sum = simd.wrapping_add(a, b);
        (sum, simd.add_overflows(a, b, sum))
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
    out: &mut [MaybeUninit<i32>; 64],
) -> bool {
    dispatch(simd, lhs, rhs, mask, out, |a, b| {
        let difference = simd.wrapping_sub(a, b);
        (difference, simd.sub_overflows(a, b, difference))
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
unsafe fn mul_lanes(
    simd: Simd,
    lhs: Side<'_>,
    rhs: Side<'_>,
    mask: u64,
    out: &mut [MaybeUninit<i32>; 64],
) -> bool {
    dispatch(simd, lhs, rhs, mask, out, |a, b| {
        let product = simd.wrapping_mul(a, b);
        (product, simd.mul_overflows(a, b, product))
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
unsafe fn div_lanes(
    simd: Simd,
    lhs: Side<'_>,
    divisor: &Divisor,
    out: &mut [MaybeUninit<i32>; 64],
) {
    let multiplier = simd.multiplier(divisor);
    map(simd, lhs, out, |n| simd.quotient(&multiplier, n))
}

#[cfg_attr(
    all(target_arch = "aarch64", not(miri)),
    target_feature(enable = "neon")
)]
#[cfg_attr(
    all(target_arch = "x86_64", not(miri)),
    target_feature(enable = "avx2")
)]
unsafe fn rem_lanes(
    simd: Simd,
    lhs: Side<'_>,
    divisor: &Divisor,
    out: &mut [MaybeUninit<i32>; 64],
) {
    let multiplier = simd.multiplier(divisor);
    map(simd, lhs, out, |n| {
        let product = simd.wrapping_mul(simd.quotient(&multiplier, n), simd.splat(divisor.value));
        simd.wrapping_sub(n, product)
    })
}

/// Every group of the column operand through `f`, stored.
#[inline]
#[cfg_attr(
    all(target_arch = "aarch64", not(miri)),
    target_feature(enable = "neon")
)]
#[cfg_attr(
    all(target_arch = "x86_64", not(miri)),
    target_feature(enable = "avx2")
)]
fn map<V: Vectors>(
    v: V,
    lhs: Side<'_>,
    out: &mut [MaybeUninit<i32>; 64],
    f: impl Fn(V::I32) -> V::I32,
) {
    match lhs {
        Side::Dense(a) => store(v, out, dense(v, a), f),
        Side::Datum(a) => store(v, out, datum(v, a), f),
        Side::Scalar(_) => unreachable!("a scalar dividend"),
    }
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
fn store<V: Vectors>(
    v: V,
    out: &mut [MaybeUninit<i32>; 64],
    load: impl Fn(usize) -> V::I32,
    f: impl Fn(V::I32) -> V::I32,
) {
    let base = out.as_mut_ptr().cast::<i32>();
    for group in 0..64 / V::LANES {
        // SAFETY: `group` is below `64 / LANES`, so the values written end
        // within the word; MaybeUninit<i32> has i32's layout.
        unsafe { v.store(base.add(group * V::LANES), f(load(group))) };
    }
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
    out: &mut [MaybeUninit<i32>; 64],
    op: impl Fn(V::I32, V::I32) -> (V::I32, V::M32),
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

/// A loader of the groups of a dense block.
#[inline]
#[cfg_attr(
    all(target_arch = "aarch64", not(miri)),
    target_feature(enable = "neon")
)]
#[cfg_attr(
    all(target_arch = "x86_64", not(miri)),
    target_feature(enable = "avx2")
)]
fn dense<V: Vectors>(v: V, values: &[i32; 64]) -> impl Fn(usize) -> V::I32 + '_ {
    // SAFETY: `apply` and `store` ask for the groups of a word only.
    move |group| unsafe { v.load(values, group) }
}

/// A loader of the groups of a Datum block.
#[inline]
#[cfg_attr(
    all(target_arch = "aarch64", not(miri)),
    target_feature(enable = "neon")
)]
#[cfg_attr(
    all(target_arch = "x86_64", not(miri)),
    target_feature(enable = "avx2")
)]
fn datum<V: Vectors>(v: V, values: &[u64; 64]) -> impl Fn(usize) -> V::I32 + '_ {
    // SAFETY: as in `dense`.
    move |group| unsafe { v.load_datums(values, group) }
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
fn scalar<V: Vectors>(v: V, value: i32) -> impl Fn(usize) -> V::I32 {
    let lanes = v.splat(value);
    move |_| lanes
}

/// Every group: operate, store, and keep the overflow lanes that the mask
/// selects.
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
    out: &mut [MaybeUninit<i32>; 64],
    op: &impl Fn(V::I32, V::I32) -> (V::I32, V::M32),
    left: impl Fn(usize) -> V::I32,
    right: impl Fn(usize) -> V::I32,
) -> bool {
    let mut overflow = v.none();
    let base = out.as_mut_ptr().cast::<i32>();
    if mask == u64::MAX {
        for group in 0..64 / V::LANES {
            let (result, over) = op(left(group), right(group));
            overflow = v.or(overflow, over);
            // SAFETY: `group` is below `64 / LANES`, so the values written
            // end within the word; MaybeUninit<i32> has i32's layout.
            unsafe { v.store(base.add(group * V::LANES), result) };
        }
    } else {
        for byte in 0..8 {
            let masks = v.lane_masks((mask >> (byte * 8)) as u8);
            for (group, lanes) in (byte * (8 / V::LANES)..).zip(masks) {
                let (result, over) = op(left(group), right(group));
                overflow = v.or(overflow, v.and(over, lanes));
                // SAFETY: as above.
                unsafe { v.store(base.add(group * V::LANES), result) };
            }
        }
    }
    v.any(overflow)
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use std::mem::MaybeUninit;

    use anyhow::Result;
    use proptest::collection::vec;
    use tessera_testing::{integer, property};

    use super::{div, rem};
    use crate::int32::divisor::tests::{any_divisor, dividends, divisors};
    use crate::int32::{Divisor, Side};
    use crate::simd::{Simd, Token};

    fn written(out: &[MaybeUninit<i32>; 64]) -> [i32; 64] {
        // SAFETY: the kernels write every lane.
        out.map(|lane| unsafe { lane.assume_init() })
    }

    /// The vector formula equals the scalar one, which is checked against
    /// the operators, on both storages and every test divisor.
    #[test]
    fn vector_lanes_match_the_scalar_formula() {
        for d in divisors() {
            for block in dividends(d).chunks(64) {
                let mut dense = [0; 64];
                dense[..block.len()].copy_from_slice(block);
                lanes_match_the_scalar_formula(d, dense);
            }
        }
    }

    /// Any word of dividends leaning to the edges, by any prepared divisor.
    #[test]
    fn any_word_matches_the_scalar_formula() {
        let cases = (any_divisor(), vec(integer::<i32>(), 64));
        property(cases, |(d, dividends)| -> Result<()> {
            lanes_match_the_scalar_formula(d, dividends.try_into().unwrap());
            Ok(())
        });
    }

    fn lanes_match_the_scalar_formula(d: i32, dense: [i32; 64]) {
        let Some(simd) = Simd::get() else {
            return;
        };
        let divisor = Divisor::new(d).unwrap();
        // PostgreSQL's Int32GetDatum sign-extends.
        let datums = dense.map(|n| i64::from(n) as u64);
        let quotients = dense.map(|n| divisor.quotient(n));
        let remainders = dense.map(|n| divisor.remainder(n));
        for lhs in [Side::Dense(&dense), Side::Datum(&datums)] {
            let mut out = [MaybeUninit::uninit(); 64];
            div(simd, lhs, &divisor, &mut out);
            assert_eq!(written(&out), quotients, "{d}");
            rem(simd, lhs, &divisor, &mut out);
            assert_eq!(written(&out), remainders, "{d}");
        }
    }
}
