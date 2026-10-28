//! The primitives of AVX2: [`Vectors`] for [`Avx2`], eight int4 lanes to
//! a vector and eight groups to a word, four int8 lanes to a vector and 16
//! vectors to a word. Each is an intrinsic, or a short
//! sequence of them, called in an `unsafe` block whose reason is the
//! token: a value of `Avx2` exists only where the processor runs AVX2.
//!
//! AVX2 is chosen at run time, so code compiled without it gets it only by
//! being inlined into a function that carries it: every primitive is
//! `#[inline(always)]`. A primitive of more than one intrinsic is a call
//! of a function compiled for AVX2, `#[inline]`, below. Inlined into a
//! kernel it is the intrinsics; from code without the feature, such as a
//! driver asking for the non-NULL rows or a closure not yet inlined into
//! its kernel, it is one call rather than one for each intrinsic, which
//! cannot be inlined there and made a loader too dear to inline.

use core::arch::x86_64::{
    __m256i, _mm256_add_epi32, _mm256_and_si256, _mm256_blend_epi32, _mm256_castps_si256,
    _mm256_castsi256_pd, _mm256_castsi256_ps, _mm256_cmpeq_epi8, _mm256_cmpeq_epi32,
    _mm256_cmpeq_epi64, _mm256_cmpgt_epi32, _mm256_cmpgt_epi64, _mm256_loadu_si256,
    _mm256_movemask_epi8, _mm256_movemask_pd, _mm256_movemask_ps, _mm256_mul_epi32,
    _mm256_mullo_epi32, _mm256_or_si256, _mm256_permute4x64_epi64, _mm256_set1_epi32,
    _mm256_set1_epi64x, _mm256_setr_epi32, _mm256_setzero_si256, _mm256_shuffle_ps,
    _mm256_srai_epi32, _mm256_srav_epi32, _mm256_srli_epi64, _mm256_storeu_si256, _mm256_sub_epi32,
    _mm256_testz_si256, _mm256_xor_si256,
};

use super::{Avx2, Vectors};
use crate::int32::Divisor;

impl Vectors for Avx2 {
    type I32 = __m256i;
    type M32 = __m256i;
    const LANES: usize = 8;

    #[inline(always)]
    unsafe fn load(self, values: &[i32; 64], group: usize) -> __m256i {
        debug_assert!(group < 64 / Self::LANES);
        // SAFETY: the token proves that this processor runs AVX2, and the
        // caller keeps `group` below 8, so the eight lanes read end within
        // the array; the load takes any alignment.
        unsafe { _mm256_loadu_si256(values.as_ptr().add(group * 8).cast()) }
    }

    #[inline(always)]
    unsafe fn load_datums(self, values: &[u64; 64], group: usize) -> __m256i {
        debug_assert!(group < 64 / Self::LANES);
        // SAFETY: as in `load`, for the eight Datums of the group.
        unsafe { load_datums(values, group) }
    }

    #[inline(always)]
    fn splat(self, value: i32) -> __m256i {
        // SAFETY: the token proves that this processor runs AVX2.
        unsafe { _mm256_set1_epi32(value) }
    }

    #[inline(always)]
    fn eq(self, a: __m256i, b: __m256i) -> __m256i {
        // SAFETY: as in `splat`.
        unsafe { _mm256_cmpeq_epi32(a, b) }
    }

    #[inline(always)]
    fn gt(self, a: __m256i, b: __m256i) -> __m256i {
        // SAFETY: as in `splat`.
        unsafe { _mm256_cmpgt_epi32(a, b) }
    }

    #[inline(always)]
    fn not(self, mask: __m256i) -> __m256i {
        // SAFETY: as in `splat`.
        unsafe { not(mask) }
    }

    #[inline(always)]
    fn pack(self, lanes: impl Fn(usize) -> __m256i) -> u64 {
        // SAFETY: as in `splat`.
        unsafe { pack_lanes(lanes) }
    }

    #[inline(always)]
    fn non_null_bits(self, isnull: &[bool; 64]) -> u64 {
        // SAFETY: as in `splat`.
        unsafe { non_null_lanes(isnull) }
    }

    #[inline(always)]
    unsafe fn store(self, at: *mut i32, lanes: __m256i) {
        // SAFETY: the token proves that this processor runs AVX2, and the
        // caller keeps `at` valid for the eight values written, which take
        // any alignment.
        unsafe { _mm256_storeu_si256(at.cast(), lanes) }
    }

    type Masks8 = [__m256i; 1];

    #[inline(always)]
    fn lane_masks(self, bits: u8) -> [__m256i; 1] {
        // SAFETY: as in `splat`.
        [unsafe { lane_masks(bits) }]
    }

    #[inline(always)]
    fn none(self) -> __m256i {
        // SAFETY: as in `splat`.
        unsafe { _mm256_setzero_si256() }
    }

    #[inline(always)]
    fn and(self, a: __m256i, b: __m256i) -> __m256i {
        // SAFETY: as in `splat`.
        unsafe { _mm256_and_si256(a, b) }
    }

    #[inline(always)]
    fn or(self, a: __m256i, b: __m256i) -> __m256i {
        // SAFETY: as in `splat`.
        unsafe { _mm256_or_si256(a, b) }
    }

    #[inline(always)]
    fn any(self, mask: __m256i) -> bool {
        // SAFETY: as in `splat`.
        unsafe { _mm256_testz_si256(mask, mask) == 0 }
    }

    #[inline(always)]
    fn wrapping_add(self, a: __m256i, b: __m256i) -> __m256i {
        // SAFETY: as in `splat`.
        unsafe { _mm256_add_epi32(a, b) }
    }

    #[inline(always)]
    fn wrapping_sub(self, a: __m256i, b: __m256i) -> __m256i {
        // SAFETY: as in `splat`.
        unsafe { _mm256_sub_epi32(a, b) }
    }

    #[inline(always)]
    fn wrapping_mul(self, a: __m256i, b: __m256i) -> __m256i {
        // SAFETY: as in `splat`.
        unsafe { _mm256_mullo_epi32(a, b) }
    }

    #[inline(always)]
    fn add_overflows(self, a: __m256i, b: __m256i, sum: __m256i) -> __m256i {
        // SAFETY: as in `splat`.
        unsafe { add_overflows(a, b, sum) }
    }

    #[inline(always)]
    fn sub_overflows(self, a: __m256i, b: __m256i, difference: __m256i) -> __m256i {
        // SAFETY: as in `splat`.
        unsafe { sub_overflows(a, b, difference) }
    }

    #[inline(always)]
    fn mul_overflows(self, a: __m256i, b: __m256i, product: __m256i) -> __m256i {
        // SAFETY: as in `splat`.
        unsafe { mul_overflows(a, b, product) }
    }

    type Multiplier = Multiplier;

    #[inline(always)]
    fn multiplier(self, divisor: &Divisor) -> Multiplier {
        // SAFETY: as in `splat`.
        unsafe { Multiplier::new(divisor) }
    }

    #[inline(always)]
    fn quotient(self, multiplier: &Multiplier, n: __m256i) -> __m256i {
        // SAFETY: as in `splat`.
        unsafe { multiplier.quotient(n) }
    }

    type I64 = __m256i;
    type M64 = __m256i;
    const LANES64: usize = 4;

    #[inline(always)]
    unsafe fn load64(self, values: &[i64; 64], index: usize) -> __m256i {
        debug_assert!(index < 64 / Self::LANES64);
        // SAFETY: the token proves that this processor runs AVX2, and the
        // caller keeps `index` below 16, so the four lanes read end within
        // the array; the load takes any alignment.
        unsafe { _mm256_loadu_si256(values.as_ptr().add(index * 4).cast()) }
    }

    #[inline(always)]
    unsafe fn load_datums64(self, values: &[u64; 64], index: usize) -> __m256i {
        debug_assert!(index < 64 / Self::LANES64);
        // SAFETY: as in `load64`; a Datum holding an int8 is its bits.
        unsafe { _mm256_loadu_si256(values.as_ptr().add(index * 4).cast()) }
    }

    #[inline(always)]
    fn splat64(self, value: i64) -> __m256i {
        // SAFETY: as in `splat`.
        unsafe { _mm256_set1_epi64x(value) }
    }

    #[inline(always)]
    fn eq64(self, a: __m256i, b: __m256i) -> __m256i {
        // SAFETY: as in `splat`.
        unsafe { _mm256_cmpeq_epi64(a, b) }
    }

    #[inline(always)]
    fn gt64(self, a: __m256i, b: __m256i) -> __m256i {
        // SAFETY: as in `splat`.
        unsafe { _mm256_cmpgt_epi64(a, b) }
    }

    #[inline(always)]
    fn not64(self, mask: __m256i) -> __m256i {
        // SAFETY: as in `splat`.
        unsafe { not(mask) }
    }

    #[inline(always)]
    fn pack64(self, invert: bool, lanes: impl Fn(usize) -> __m256i) -> u64 {
        // SAFETY: as in `splat`.
        unsafe { pack64_lanes(invert, lanes) }
    }
}

/// The rows of a Datum block whose flag is false: the flags compared with
/// zero 32 at a time, `vpmovmskb` taking a bit of each byte. Inlined into
/// the kernels, called from the drivers.
#[inline]
#[target_feature(enable = "avx2")]
fn non_null_lanes(isnull: &[bool; 64]) -> u64 {
    let flags = isnull.as_ptr().cast::<__m256i>();
    let zero = _mm256_setzero_si256();
    // SAFETY: a bool is one byte holding 0 or 1, the two loads of 32 bytes
    // end within the 64 flags, and they take any alignment.
    let (low, high) = unsafe { (_mm256_loadu_si256(flags), _mm256_loadu_si256(flags.add(1))) };
    let low = _mm256_movemask_epi8(_mm256_cmpeq_epi8(low, zero));
    let high = _mm256_movemask_epi8(_mm256_cmpeq_epi8(high, zero));
    u64::from(low as u32) | u64::from(high as u32) << 32
}

/// The low halves of eight Datums, the int4 values of a group. Two loads
/// of four Datums: the low halves are the even 32-bit lanes, `vshufps`
/// takes those of both loads in each half of the vector, rows 0, 1, 4, 5
/// and then 2, 3, 6, 7, and `vpermq` puts the pairs of rows in order.
///
/// # Safety
///
/// `group` must be below 8.
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn load_datums(values: &[u64; 64], group: usize) -> __m256i {
    // SAFETY: the caller keeps `group` below 8, so the eight Datums read
    // end within the array; the loads take any alignment.
    let (first, second) = unsafe {
        let start = values.as_ptr().add(group * 8);
        (
            _mm256_loadu_si256(start.cast()),
            _mm256_loadu_si256(start.add(4).cast()),
        )
    };
    let lows =
        _mm256_shuffle_ps::<0b10_00_10_00>(_mm256_castsi256_ps(first), _mm256_castsi256_ps(second));
    _mm256_permute4x64_epi64::<0b11_01_10_00>(_mm256_castps_si256(lows))
}

/// The lanes where `mask` does not hold.
#[inline]
#[target_feature(enable = "avx2")]
fn not(mask: __m256i) -> __m256i {
    _mm256_xor_si256(mask, _mm256_set1_epi32(-1))
}

/// The lane masks of eight rows from their byte of bits: each lane keeps
/// the bit of its row and compares equal to it where it is set.
#[inline]
#[target_feature(enable = "avx2")]
fn lane_masks(bits: u8) -> __m256i {
    let weights = _mm256_setr_epi32(1, 1 << 1, 1 << 2, 1 << 3, 1 << 4, 1 << 5, 1 << 6, 1 << 7);
    let bits = _mm256_set1_epi32(i32::from(bits));
    _mm256_cmpeq_epi32(_mm256_and_si256(bits, weights), weights)
}

/// A sum overflows where it is below `a` though `b` is not negative, or
/// not below `a` though `b` is negative: AVX2 has no saturating 32-bit
/// addition.
#[inline]
#[target_feature(enable = "avx2")]
fn add_overflows(a: __m256i, b: __m256i, sum: __m256i) -> __m256i {
    let negative = _mm256_cmpgt_epi32(_mm256_setzero_si256(), b);
    _mm256_xor_si256(_mm256_cmpgt_epi32(a, sum), negative)
}

/// A difference overflows where it is above `a` though `b` is not
/// negative, or not above `a` though `b` is negative.
#[inline]
#[target_feature(enable = "avx2")]
fn sub_overflows(a: __m256i, b: __m256i, difference: __m256i) -> __m256i {
    let negative = _mm256_cmpgt_epi32(_mm256_setzero_si256(), b);
    _mm256_xor_si256(_mm256_cmpgt_epi32(difference, a), negative)
}

/// A product fits where the high half of its 64-bit form is the sign of
/// its low half, the wrapped lane.
#[inline]
#[target_feature(enable = "avx2")]
fn mul_overflows(a: __m256i, b: __m256i, product: __m256i) -> __m256i {
    let fits = _mm256_cmpeq_epi32(mul_high(a, b), _mm256_srai_epi32::<31>(product));
    not(fits)
}

/// The high halves of the 64-bit products of the lanes of `a` and `b`:
/// `vpmuldq` multiplies the even lanes, and again with the odd lanes
/// shifted down; the high halves of the even products are shifted down
/// into their lanes, and those of the odd ones are in theirs.
#[inline]
#[target_feature(enable = "avx2")]
fn mul_high(a: __m256i, b: __m256i) -> __m256i {
    let even = _mm256_mul_epi32(a, b);
    let odd = _mm256_mul_epi32(_mm256_srli_epi64::<32>(a), _mm256_srli_epi64::<32>(b));
    _mm256_blend_epi32::<0b1010_1010>(_mm256_srli_epi64::<32>(even), odd)
}

/// The bits of a word from the lane masks of its eight groups: the sign
/// bit of each lane is its mask's bit, and `vmovmskps` gives the eight bits
/// of a group at once.
#[inline]
#[target_feature(enable = "avx2")]
fn pack_lanes(lanes: impl Fn(usize) -> __m256i) -> u64 {
    let mut bits = 0;
    for group in 0..8 {
        let mask = _mm256_movemask_ps(_mm256_castsi256_ps(lanes(group)));
        bits |= u64::from(mask as u8) << (group * 8);
    }
    bits
}

/// The bits of a word from the lane masks of its 16 vectors of int8
/// lanes: `vmovmskpd` gives the four bits of a vector at once, and an
/// inversion is one of the word.
#[inline]
#[target_feature(enable = "avx2")]
fn pack64_lanes(invert: bool, lanes: impl Fn(usize) -> __m256i) -> u64 {
    let mut bits = 0;
    for index in 0..16 {
        let mask = _mm256_movemask_pd(_mm256_castsi256_pd(lanes(index)));
        bits |= u64::from(mask as u8) << (index * 4);
    }
    if invert { !bits } else { bits }
}

/// A prepared divisor's constants as lanes, made once per word.
pub(crate) struct Multiplier {
    magic: __m256i,
    bias: __m256i,
    /// The shift in every lane, for `vpsravd`.
    shift: __m256i,
    sign: __m256i,
}

impl Multiplier {
    #[inline]
    #[target_feature(enable = "avx2")]
    fn new(divisor: &Divisor) -> Self {
        Self {
            magic: _mm256_set1_epi32(divisor.magic),
            bias: _mm256_set1_epi32(divisor.bias),
            shift: _mm256_set1_epi32(divisor.shift as i32),
            sign: _mm256_set1_epi32(divisor.sign),
        }
    }

    /// `Divisor::quotient` on eight lanes.
    #[inline]
    #[target_feature(enable = "avx2")]
    fn quotient(&self, n: __m256i) -> __m256i {
        let mut q = _mm256_add_epi32(mul_high(n, self.magic), n);
        q = _mm256_add_epi32(q, _mm256_and_si256(_mm256_srai_epi32::<31>(q), self.bias));
        q = _mm256_srav_epi32(q, self.shift);
        _mm256_sub_epi32(_mm256_xor_si256(q, self.sign), self.sign)
    }
}
