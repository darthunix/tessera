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
    int32x4_t, int64x2_t, uint32x4_t, uint64x2_t, vaddlvq_u8, vaddq_s32, vaddq_s64, vaddq_u8,
    vaddvq_s64, vandq_s32, vandq_u8, vandq_u32, vandq_u64, vbicq_u64, vbslq_s32, vbslq_s64,
    vceqq_s32, vceqq_s64, vceqzq_u8, vcgeq_s32, vcgeq_s64, vcgtq_s32, vcgtq_s64, vcleq_s32,
    vcleq_s64, vcltq_s32, vcltq_s64, vcombine_u8, vcombine_u32, vdup_n_u8, vdupq_n_s32,
    vdupq_n_s64, vdupq_n_u8, vdupq_n_u32, vdupq_n_u64, veorq_s32, vget_low_s32, vgetq_lane_s64,
    vld1q_s32, vld1q_s64, vld1q_u8, vmaxq_s32, vmaxvq_s32, vmaxvq_u32, vminq_s32, vminvq_s32,
    vminvq_u32, vmovl_high_s32, vmovl_s32, vmovn_u64, vmull_high_n_s32, vmull_high_s32,
    vmull_n_s32, vmull_s32, vmulq_s32, vmvnq_u32, vorrq_u32, vorrq_u64, vpadalq_s32, vqaddq_s32,
    vqaddq_s64, vqsubq_s32, vqsubq_s64, vreinterpretq_s32_s64, vreinterpretq_s32_u32,
    vreinterpretq_u32_u64, vreinterpretq_u64_u32, vshlq_s32, vshrq_n_s32, vst1q_s32, vst1q_s64,
    vsubq_s32, vsubq_s64, vtst_u8, vuzp2q_s32,
};

use super::{
    Neon, Vectors, byte_weights, lane_masks, load_datums, non_null_lanes, pack_lanes, widen,
};
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

    #[inline(always)]
    fn count_bits(self, bits: u64) -> usize {
        bits.count_ones() as usize
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

    /// Two bytes of the bits make the masks of the four groups, two each.
    #[inline]
    fn lane_masks4(self, bits: u64, quad: usize) -> [uint32x4_t; 4] {
        // SAFETY: as in `splat`.
        unsafe {
            let weights = byte_weights();
            let (m0, m1) = lane_masks((bits >> (quad * 16)) as u8, weights);
            let (m2, m3) = lane_masks((bits >> (quad * 16 + 8)) as u8, weights);
            [m0, m1, m2, m3]
        }
    }

    #[inline]
    fn keep(self, lanes: int32x4_t, mask: uint32x4_t) -> int32x4_t {
        // SAFETY: as in `splat`.
        unsafe { vandq_s32(lanes, vreinterpretq_s32_u32(mask)) }
    }

    #[inline]
    fn select(self, mask: uint32x4_t, a: int32x4_t, b: int32x4_t) -> int32x4_t {
        // SAFETY: as in `splat`.
        unsafe { vbslq_s32(mask, a, b) }
    }

    #[inline]
    fn min(self, a: int32x4_t, b: int32x4_t) -> int32x4_t {
        // SAFETY: as in `splat`.
        unsafe { vminq_s32(a, b) }
    }

    #[inline]
    fn max(self, a: int32x4_t, b: int32x4_t) -> int32x4_t {
        // SAFETY: as in `splat`.
        unsafe { vmaxq_s32(a, b) }
    }

    #[inline]
    fn min_across(self, lanes: int32x4_t) -> i32 {
        // SAFETY: as in `splat`.
        unsafe { vminvq_s32(lanes) }
    }

    #[inline]
    fn max_across(self, lanes: int32x4_t) -> i32 {
        // SAFETY: as in `splat`.
        unsafe { vmaxvq_s32(lanes) }
    }

    /// `sadalp`: the pairwise sums of the four lanes, widened, added in.
    #[inline]
    fn widen_add(self, acc: int64x2_t, lanes: int32x4_t) -> int64x2_t {
        // SAFETY: as in `splat`.
        unsafe { vpadalq_s32(acc, lanes) }
    }

    #[inline]
    fn count_non_null(self, isnull: &[bool; 64], selected: u64) -> usize {
        // SAFETY: as in `splat`.
        unsafe { count_non_null_lanes(isnull, selected) }
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

    #[inline]
    unsafe fn store64(self, at: *mut i64, lanes: int64x2_t) {
        // SAFETY: the token proves that this processor runs NEON, and the
        // caller keeps `at` valid for the two values written.
        unsafe { vst1q_s64(at, lanes) }
    }

    type Masks64 = [uint64x2_t; 4];

    /// The masks of the two groups of four rows, each widened to two pairs.
    #[inline]
    fn lane_masks64(self, bits: u8) -> [uint64x2_t; 4] {
        // SAFETY: as in `splat`.
        unsafe {
            let (first, second) = lane_masks(bits, byte_weights());
            let (m0, m1) = widen(first);
            let (m2, m3) = widen(second);
            [m0, m1, m2, m3]
        }
    }

    /// Four vectors of two lanes are the eight rows of a byte.
    #[inline]
    fn lane_masks4_64(self, bits: u64, quad: usize) -> [uint64x2_t; 4] {
        self.lane_masks64((bits >> (quad * 8)) as u8)
    }

    #[inline]
    fn select64(self, mask: uint64x2_t, a: int64x2_t, b: int64x2_t) -> int64x2_t {
        // SAFETY: as in `splat`.
        unsafe { vbslq_s64(mask, a, b) }
    }

    #[inline]
    fn none64(self) -> uint64x2_t {
        // SAFETY: as in `splat`.
        unsafe { vdupq_n_u64(0) }
    }

    #[inline]
    fn every64(self) -> uint64x2_t {
        // SAFETY: as in `splat`.
        unsafe { vdupq_n_u64(u64::MAX) }
    }

    #[inline]
    fn and64(self, a: uint64x2_t, b: uint64x2_t) -> uint64x2_t {
        // SAFETY: as in `splat`.
        unsafe { vandq_u64(a, b) }
    }

    #[inline]
    fn or64(self, a: uint64x2_t, b: uint64x2_t) -> uint64x2_t {
        // SAFETY: as in `splat`.
        unsafe { vorrq_u64(a, b) }
    }

    #[inline]
    fn and_not64(self, a: uint64x2_t, b: uint64x2_t) -> uint64x2_t {
        // SAFETY: as in `splat`.
        unsafe { vbicq_u64(a, b) }
    }

    #[inline]
    fn any64(self, mask: uint64x2_t) -> bool {
        // SAFETY: as in `splat`.
        unsafe { vmaxvq_u32(vreinterpretq_u32_u64(mask)) != 0 }
    }

    #[inline]
    fn all64(self, mask: uint64x2_t) -> bool {
        // SAFETY: as in `splat`.
        unsafe { vminvq_u32(vreinterpretq_u32_u64(mask)) != 0 }
    }

    #[inline]
    fn wrapping_add64(self, a: int64x2_t, b: int64x2_t) -> int64x2_t {
        // SAFETY: as in `splat`.
        unsafe { vaddq_s64(a, b) }
    }

    #[inline]
    fn wrapping_sub64(self, a: int64x2_t, b: int64x2_t) -> int64x2_t {
        // SAFETY: as in `splat`.
        unsafe { vsubq_s64(a, b) }
    }

    /// The saturating sum equals the wrapped one where it fits.
    #[inline]
    fn add_fits64(self, a: int64x2_t, b: int64x2_t, sum: int64x2_t) -> uint64x2_t {
        // SAFETY: as in `splat`.
        unsafe { vceqq_s64(sum, vqaddq_s64(a, b)) }
    }

    /// As in `add_fits64`, with the saturating difference.
    #[inline]
    fn sub_fits64(self, a: int64x2_t, b: int64x2_t, difference: int64x2_t) -> uint64x2_t {
        // SAFETY: as in `splat`.
        unsafe { vceqq_s64(difference, vqsubq_s64(a, b)) }
    }

    #[inline]
    fn sum_across64(self, lanes: int64x2_t) -> i64 {
        // SAFETY: as in `splat`.
        unsafe { vaddvq_s64(lanes) }
    }

    /// NEON has no least of int8 lanes: the two are compared in scalar
    /// code.
    #[inline]
    fn min_across64(self, lanes: int64x2_t) -> i64 {
        // SAFETY: as in `splat`.
        unsafe { vgetq_lane_s64::<0>(lanes).min(vgetq_lane_s64::<1>(lanes)) }
    }

    /// As in `min_across64`.
    #[inline]
    fn max_across64(self, lanes: int64x2_t) -> i64 {
        // SAFETY: as in `splat`.
        unsafe { vgetq_lane_s64::<0>(lanes).max(vgetq_lane_s64::<1>(lanes)) }
    }
}

/// Selected non-NULL rows of a Datum block: the flags are compared with
/// zero and counted as bytes, so that no `addv` per eight rows is needed.
#[inline]
#[target_feature(enable = "neon")]
fn count_non_null_lanes(isnull: &[bool; 64], selected: u64) -> usize {
    let weights = byte_weights();
    let one = vdupq_n_u8(1);
    // Per-lane counts stay at most four.
    let mut counts = vdupq_n_u8(0);
    for quarter in 0..4 {
        // SAFETY: a bool is one byte holding 0 or 1, and the sixteen bytes
        // read end within the array.
        let flags = unsafe { vld1q_u8(isnull.as_ptr().cast::<u8>().add(quarter * 16)) };
        let mut present = vceqzq_u8(flags);
        if selected != u64::MAX {
            let bits = (selected >> (quarter * 16)) as u16;
            let low = vtst_u8(vdup_n_u8(bits as u8), weights);
            let high = vtst_u8(vdup_n_u8((bits >> 8) as u8), weights);
            present = vandq_u8(present, vcombine_u8(low, high));
        }
        counts = vaddq_u8(counts, vandq_u8(present, one));
    }
    usize::from(vaddlvq_u8(counts))
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
