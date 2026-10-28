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
    __m256i, _mm256_castps_si256, _mm256_castsi256_pd, _mm256_castsi256_ps, _mm256_cmpeq_epi8,
    _mm256_cmpeq_epi32, _mm256_cmpeq_epi64, _mm256_cmpgt_epi32, _mm256_cmpgt_epi64,
    _mm256_loadu_si256, _mm256_movemask_epi8, _mm256_movemask_pd, _mm256_movemask_ps,
    _mm256_permute4x64_epi64, _mm256_set1_epi32, _mm256_set1_epi64x, _mm256_setzero_si256,
    _mm256_shuffle_ps, _mm256_xor_si256,
};

use super::{Avx2, Vectors};

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
