//! The primitives of NEON: [`Vectors`] for [`Neon`], four int4 lanes to a
//! vector and 16 groups to a word, two int8 lanes to a vector and 32
//! vectors to a word. Each is an intrinsic, or a helper of
//! this module made of them, called in an `unsafe` block whose reason is
//! the token: a value of `Neon` exists only where the processor runs NEON.
//!
//! Every function of AArch64 carries NEON, so the primitives are
//! `#[inline]`, as the intrinsics are: `#[inline(always)]` inlined them
//! earlier than the intrinsics had been, and the NEON code of the kernels
//! moved here changed its loads and spills.

use core::arch::aarch64::{
    int32x4_t, int64x2_t, uint32x4_t, uint64x2_t, vaddq_s32, vandq_s32, vandq_u32, vceqq_s32,
    vceqq_s64, vcgeq_s32, vcgeq_s64, vcgtq_s32, vcgtq_s64, vcleq_s32, vcleq_s64, vcltq_s32,
    vcltq_s64, vcombine_u32, vdupq_n_s32, vdupq_n_s64, vdupq_n_u32, veorq_s32, vget_low_s32,
    vld1q_s32, vld1q_s64, vmaxvq_u32, vmovl_high_s32, vmovl_s32, vmovn_u64, vmull_high_n_s32,
    vmull_high_s32, vmull_n_s32, vmull_s32, vmulq_s32, vmvnq_u32, vorrq_u32, vqaddq_s32,
    vqsubq_s32, vreinterpretq_s32_s64, vreinterpretq_u32_u64, vreinterpretq_u64_u32, vshlq_s32,
    vshrq_n_s32, vst1q_s32, vsubq_s32, vuzp2q_s32,
};

use super::{Neon, Vectors, byte_weights, lane_masks, load_datums, non_null_lanes, pack_lanes};
use crate::int32::Divisor;

impl Vectors for Neon {
    type I32 = int32x4_t;
    type M32 = uint32x4_t;
    const LANES: usize = 4;

    #[inline]
    unsafe fn load(self, values: &[i32; 64], group: usize) -> int32x4_t {
        debug_assert!(group < 64 / Self::LANES);
        // SAFETY: the token proves that this processor runs NEON, and the
        // caller keeps `group` below 16, so the four lanes read end within
        // the array.
        unsafe { vld1q_s32(values.as_ptr().add(group * 4)) }
    }

    #[inline]
    unsafe fn load_datums(self, values: &[u64; 64], group: usize) -> int32x4_t {
        debug_assert!(group < 64 / Self::LANES);
        // SAFETY: as in `load`, for the four Datums of the group.
        unsafe { load_datums(values, group) }
    }

    // NEON compares for each relation in one instruction, so each is its
    // own rather than the trait's composition.

    #[inline]
    fn splat(self, value: i32) -> int32x4_t {
        // SAFETY: the token proves that this processor runs NEON.
        unsafe { vdupq_n_s32(value) }
    }

    #[inline]
    fn eq(self, a: int32x4_t, b: int32x4_t) -> uint32x4_t {
        // SAFETY: as in `splat`.
        unsafe { vceqq_s32(a, b) }
    }

    #[inline]
    fn gt(self, a: int32x4_t, b: int32x4_t) -> uint32x4_t {
        // SAFETY: as in `splat`.
        unsafe { vcgtq_s32(a, b) }
    }

    #[inline]
    fn ge(self, a: int32x4_t, b: int32x4_t) -> uint32x4_t {
        // SAFETY: as in `splat`.
        unsafe { vcgeq_s32(a, b) }
    }

    #[inline]
    fn lt(self, a: int32x4_t, b: int32x4_t) -> uint32x4_t {
        // SAFETY: as in `splat`.
        unsafe { vcltq_s32(a, b) }
    }

    #[inline]
    fn le(self, a: int32x4_t, b: int32x4_t) -> uint32x4_t {
        // SAFETY: as in `splat`.
        unsafe { vcleq_s32(a, b) }
    }

    #[inline]
    fn not(self, mask: uint32x4_t) -> uint32x4_t {
        // SAFETY: as in `splat`.
        unsafe { vmvnq_u32(mask) }
    }

    #[inline]
    fn pack(self, lanes: impl Fn(usize) -> uint32x4_t) -> u64 {
        // SAFETY: as in `splat`.
        unsafe { pack_lanes(lanes) }
    }

    #[inline]
    fn non_null_bits(self, isnull: &[bool; 64]) -> u64 {
        // SAFETY: as in `splat`.
        unsafe { non_null_lanes(isnull) }
    }

    #[inline]
    unsafe fn store(self, at: *mut i32, lanes: int32x4_t) {
        // SAFETY: the token proves that this processor runs NEON, and the
        // caller keeps `at` valid for the four values written.
        unsafe { vst1q_s32(at, lanes) }
    }

    type Masks8 = [uint32x4_t; 2];

    #[inline]
    fn lane_masks(self, bits: u8) -> [uint32x4_t; 2] {
        // SAFETY: as in `splat`.
        let (first, second) = unsafe { lane_masks(bits, byte_weights()) };
        [first, second]
    }

    #[inline]
    fn none(self) -> uint32x4_t {
        // SAFETY: as in `splat`.
        unsafe { vdupq_n_u32(0) }
    }

    #[inline]
    fn and(self, a: uint32x4_t, b: uint32x4_t) -> uint32x4_t {
        // SAFETY: as in `splat`.
        unsafe { vandq_u32(a, b) }
    }

    #[inline]
    fn or(self, a: uint32x4_t, b: uint32x4_t) -> uint32x4_t {
        // SAFETY: as in `splat`.
        unsafe { vorrq_u32(a, b) }
    }

    #[inline]
    fn any(self, mask: uint32x4_t) -> bool {
        // SAFETY: as in `splat`.
        unsafe { vmaxvq_u32(mask) != 0 }
    }

    #[inline]
    fn wrapping_add(self, a: int32x4_t, b: int32x4_t) -> int32x4_t {
        // SAFETY: as in `splat`.
        unsafe { vaddq_s32(a, b) }
    }

    #[inline]
    fn wrapping_sub(self, a: int32x4_t, b: int32x4_t) -> int32x4_t {
        // SAFETY: as in `splat`.
        unsafe { vsubq_s32(a, b) }
    }

    #[inline]
    fn wrapping_mul(self, a: int32x4_t, b: int32x4_t) -> int32x4_t {
        // SAFETY: as in `splat`.
        unsafe { vmulq_s32(a, b) }
    }

    /// The saturating sum differs from the wrapped one where it overflows.
    #[inline]
    fn add_overflows(self, a: int32x4_t, b: int32x4_t, sum: int32x4_t) -> uint32x4_t {
        // SAFETY: as in `splat`.
        unsafe { vmvnq_u32(vceqq_s32(sum, vqaddq_s32(a, b))) }
    }

    /// As in `add_overflows`, with the saturating difference.
    #[inline]
    fn sub_overflows(self, a: int32x4_t, b: int32x4_t, difference: int32x4_t) -> uint32x4_t {
        // SAFETY: as in `splat`.
        unsafe { vmvnq_u32(vceqq_s32(difference, vqsubq_s32(a, b))) }
    }

    /// The product fits where its 64-bit form equals the sign-extended
    /// 32-bit form.
    #[inline]
    fn mul_overflows(self, a: int32x4_t, b: int32x4_t, product: int32x4_t) -> uint32x4_t {
        // SAFETY: as in `splat`.
        unsafe {
            let low = vceqq_s64(
                vmull_s32(vget_low_s32(a), vget_low_s32(b)),
                vmovl_s32(vget_low_s32(product)),
            );
            let high = vceqq_s64(vmull_high_s32(a, b), vmovl_high_s32(product));
            vmvnq_u32(vcombine_u32(vmovn_u64(low), vmovn_u64(high)))
        }
    }

    type Multiplier = Multiplier;

    #[inline]
    fn multiplier(self, divisor: &Divisor) -> Multiplier {
        // SAFETY: as in `splat`.
        unsafe { Multiplier::new(divisor) }
    }

    #[inline]
    fn quotient(self, multiplier: &Multiplier, n: int32x4_t) -> int32x4_t {
        // SAFETY: as in `splat`.
        unsafe { multiplier.quotient(n) }
    }

    type I64 = int64x2_t;
    type M64 = uint64x2_t;
    const LANES64: usize = 2;

    #[inline]
    unsafe fn load64(self, values: &[i64; 64], index: usize) -> int64x2_t {
        debug_assert!(index < 64 / Self::LANES64);
        // SAFETY: as in `load`: the caller keeps `index` below 32, so the
        // two lanes read end within the array.
        unsafe { vld1q_s64(values.as_ptr().add(index * 2)) }
    }

    #[inline]
    unsafe fn load_datums64(self, values: &[u64; 64], index: usize) -> int64x2_t {
        debug_assert!(index < 64 / Self::LANES64);
        // SAFETY: as in `load64`; a Datum holding an int8 is its bits.
        unsafe { vld1q_s64(values.as_ptr().cast::<i64>().add(index * 2)) }
    }

    #[inline]
    fn splat64(self, value: i64) -> int64x2_t {
        // SAFETY: as in `splat`.
        unsafe { vdupq_n_s64(value) }
    }

    #[inline]
    fn eq64(self, a: int64x2_t, b: int64x2_t) -> uint64x2_t {
        // SAFETY: as in `splat`.
        unsafe { vceqq_s64(a, b) }
    }

    #[inline]
    fn gt64(self, a: int64x2_t, b: int64x2_t) -> uint64x2_t {
        // SAFETY: as in `splat`.
        unsafe { vcgtq_s64(a, b) }
    }

    #[inline]
    fn ge64(self, a: int64x2_t, b: int64x2_t) -> uint64x2_t {
        // SAFETY: as in `splat`.
        unsafe { vcgeq_s64(a, b) }
    }

    #[inline]
    fn lt64(self, a: int64x2_t, b: int64x2_t) -> uint64x2_t {
        // SAFETY: as in `splat`.
        unsafe { vcltq_s64(a, b) }
    }

    #[inline]
    fn le64(self, a: int64x2_t, b: int64x2_t) -> uint64x2_t {
        // SAFETY: as in `splat`.
        unsafe { vcleq_s64(a, b) }
    }

    #[inline]
    fn not64(self, mask: uint64x2_t) -> uint64x2_t {
        // SAFETY: as in `splat`.
        unsafe { vreinterpretq_u64_u32(vmvnq_u32(vreinterpretq_u32_u64(mask))) }
    }

    /// Two vectors narrow into the four 32-bit lanes of a group of four
    /// rows, which the int4 packing weighs and adds; an inversion is one of
    /// the narrowed group rather than one of each vector.
    #[inline]
    fn pack64(self, invert: bool, lanes: impl Fn(usize) -> uint64x2_t) -> u64 {
        let group = |group: usize| {
            // SAFETY: as in `splat`.
            unsafe {
                let rows =
                    vcombine_u32(vmovn_u64(lanes(group * 2)), vmovn_u64(lanes(group * 2 + 1)));
                if invert { vmvnq_u32(rows) } else { rows }
            }
        };
        // SAFETY: as in `splat`.
        unsafe { pack_lanes(group) }
    }
}

/// A prepared divisor's constants as lanes, made once per word.
pub(crate) struct Multiplier {
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
