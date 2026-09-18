//! Whole-word int4 addition, subtraction and multiplication with overflow
//! detection, and division by a prepared scalar divisor.
//!
//! Every lane is computed modulo 2^32 and stored; overflow is detected per
//! lane (saturating against wrapping results for `+` and `-`, the widening
//! product against the sign-extended narrow one for `*`), kept only for the
//! lanes of `mask`, and reported once per word. NULL and unselected lanes
//! thus get initialized placeholders and never raise. Division by a scalar
//! is a multiplication by the divisor's prepared reciprocal
//! ([`Divisor`]), which no lane can fail, so it takes no mask; division by
//! a column stays in the caller's lane loop, since NEON has no integer
//! division.

use core::arch::aarch64::{
    int32x4_t, uint32x4_t, vaddq_s32, vandq_s32, vandq_u32, vceqq_s32, vceqq_s64, vcombine_u32,
    vdupq_n_s32, vdupq_n_u32, veorq_s32, vget_low_s32, vmaxvq_u32, vmlsq_n_s32, vmovl_high_s32,
    vmovl_s32, vmovn_u64, vmull_high_n_s32, vmull_high_s32, vmull_n_s32, vmull_s32, vmulq_s32,
    vmvnq_u32, vorrq_u32, vqaddq_s32, vqsubq_s32, vreinterpretq_s32_s64, vshlq_s32, vshrq_n_s32,
    vst1q_s32, vsubq_s32, vuzp2q_s32,
};
use std::mem::MaybeUninit;

use super::{byte_weights, datum, dense, lane_masks};
use crate::int32::{Divisor, Side};

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

/// `lhs / divisor` into every lane of `out`; no lane can fail.
#[inline]
pub fn div(lhs: Side<'_>, divisor: &Divisor, out: &mut [MaybeUninit<i32>; 64]) {
    const { assert!(cfg!(target_feature = "neon")) }
    // SAFETY: as in add.
    unsafe { div_lanes(lhs, divisor, out) }
}

/// `lhs % divisor` into every lane of `out`; no lane can fail.
#[inline]
pub fn rem(lhs: Side<'_>, divisor: &Divisor, out: &mut [MaybeUninit<i32>; 64]) {
    const { assert!(cfg!(target_feature = "neon")) }
    // SAFETY: as in add.
    unsafe { rem_lanes(lhs, divisor, out) }
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

#[target_feature(enable = "neon")]
fn div_lanes(lhs: Side<'_>, divisor: &Divisor, out: &mut [MaybeUninit<i32>; 64]) {
    let multiplier = Multiplier::new(divisor);
    map(lhs, out, |n| multiplier.quotient(n))
}

#[target_feature(enable = "neon")]
fn rem_lanes(lhs: Side<'_>, divisor: &Divisor, out: &mut [MaybeUninit<i32>; 64]) {
    let multiplier = Multiplier::new(divisor);
    map(lhs, out, |n| {
        vmlsq_n_s32(n, multiplier.quotient(n), divisor.value)
    })
}

/// A prepared divisor's constants as lanes, made once per word.
struct Multiplier {
    magic: i32,
    bias: int32x4_t,
    /// Negated, so that the signed shift left shifts right arithmetically.
    shift: int32x4_t,
    sign: int32x4_t,
}

impl Multiplier {
    #[inline]
    #[target_feature(enable = "neon")]
    fn new(divisor: &Divisor) -> Self {
        Self {
            magic: divisor.magic,
            bias: vdupq_n_s32(divisor.bias),
            shift: vdupq_n_s32(-(divisor.shift as i32)),
            sign: vdupq_n_s32(divisor.sign),
        }
    }

    /// `Divisor::quotient` on four lanes: uzp2 gathers the high halves of
    /// the widening products.
    #[inline]
    #[target_feature(enable = "neon")]
    fn quotient(&self, n: int32x4_t) -> int32x4_t {
        let low = vmull_n_s32(vget_low_s32(n), self.magic);
        let high = vmull_high_n_s32(n, self.magic);
        let mut q = vuzp2q_s32(vreinterpretq_s32_s64(low), vreinterpretq_s32_s64(high));
        q = vaddq_s32(q, n);
        q = vaddq_s32(q, vandq_s32(vshrq_n_s32::<31>(q), self.bias));
        q = vshlq_s32(q, self.shift);
        vsubq_s32(veorq_s32(q, self.sign), self.sign)
    }
}

/// Every group of four lanes of the column operand through `f`, stored.
#[inline]
#[target_feature(enable = "neon")]
fn map(lhs: Side<'_>, out: &mut [MaybeUninit<i32>; 64], f: impl Fn(int32x4_t) -> int32x4_t) {
    match lhs {
        Side::Dense(a) => store(out, dense(a), f),
        Side::Datum(a) => store(out, datum(a), f),
        Side::Scalar(_) => unreachable!("a scalar dividend"),
    }
}

#[inline]
#[target_feature(enable = "neon")]
fn store(
    out: &mut [MaybeUninit<i32>; 64],
    load: impl Fn(usize) -> int32x4_t,
    f: impl Fn(int32x4_t) -> int32x4_t,
) {
    let base = out.as_mut_ptr().cast::<i32>();
    for group in 0..16 {
        // SAFETY: `group` is below 16, so the four lanes written end within
        // the array; MaybeUninit<i32> has i32's layout.
        unsafe { vst1q_s32(base.add(group * 4), f(load(group))) };
    }
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

#[cfg(test)]
mod tests {
    use std::mem::MaybeUninit;

    use super::{div, rem};
    use crate::int32::divisor::tests::{dividends, divisors};
    use crate::int32::{Divisor, Side};

    fn written(out: &[MaybeUninit<i32>; 64]) -> [i32; 64] {
        // SAFETY: the kernels write every lane.
        out.map(|lane| unsafe { lane.assume_init() })
    }

    /// The vector formula equals the scalar one, which is checked against
    /// the operators, on both storages and every test divisor.
    #[test]
    fn vector_lanes_match_the_scalar_formula() {
        for d in divisors() {
            let divisor = Divisor::new(d).unwrap();
            for block in dividends(d).chunks(64) {
                let mut dense = [0; 64];
                dense[..block.len()].copy_from_slice(block);
                // PostgreSQL's Int32GetDatum sign-extends.
                let datums = dense.map(|n| i64::from(n) as u64);
                let quotients = dense.map(|n| divisor.quotient(n));
                let remainders = dense.map(|n| divisor.remainder(n));
                for lhs in [Side::Dense(&dense), Side::Datum(&datums)] {
                    let mut out = [MaybeUninit::uninit(); 64];
                    div(lhs, &divisor, &mut out);
                    assert_eq!(written(&out), quotients, "{d}");
                    rem(lhs, &divisor, &mut out);
                    assert_eq!(written(&out), remainders, "{d}");
                }
            }
        }
    }
}
