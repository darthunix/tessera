//! Vector kernels over the storage of full prepared words.
//!
//! The vector sets are named by tokens (`token.rs`), defined on every
//! target. NEON runs on AArch64, and AVX2 on x86-64 where the processor
//! has it, for the filter of int4 and the comparison of two int4 columns;
//! the other kernels are NEON's alone, compiled only on AArch64. Under
//! Miri no set runs, and callers fall back to the row paths. Instructions
//! always drop with vectors, so a kernel here is kept on its cycle gain
//! (see the benchmark guide). This is the one module of the crate allowed
//! to use `unsafe`, for vector loads from borrowed arrays and for the
//! intrinsics; every load states why it stays in bounds.
//!
//! A kernel is written once over the primitives of [`Vectors`], which each
//! vector set implements (`neon.rs`, `avx2.rs`); the kernels not yet moved
//! call the NEON intrinsics themselves. Intrinsics are callable without `unsafe`
//! only from functions carrying the `neon` feature; closures inherit it
//! from the function that defines them, and a primitive, which carries no
//! feature, calls them in an `unsafe` block whose reason is its token.
//! Public entry points are plain functions that take the token of NEON,
//! whose value proves that the processor runs it, and make the one `unsafe`
//! call into the feature-carrying code.
#![allow(unsafe_code)]

mod token;

pub(crate) use token::{Avx2, Neon, Simd, Token};

#[cfg(all(target_arch = "x86_64", not(miri)))]
mod avx2;
#[cfg(all(target_arch = "aarch64", not(miri)))]
mod neon;

/// The primitives of a vector set, over the lanes of int4 and of int8
/// values, that the kernels of this module are written over once. A value
/// of the type is the token of its set, so a primitive may use the set's
/// instructions.
///
/// A kernel is a generic function, `#[inline(always)]`, called from one
/// function for each set that carries the set's feature and is never
/// inlined: code compiled without the feature gets it only by being
/// inlined into a function that carries it, and otherwise passes its
/// vectors through memory. A set whose feature the whole target carries,
/// as AArch64 carries NEON, marks its primitives `#[inline]`, as the
/// intrinsics they wrap are; one chosen at run time marks them
/// `#[inline(always)]`.
///
/// A word of 64 rows is `64 / LANES` groups of int4 lanes, or `64 /
/// LANES64` vectors of int8 lanes, in the order of its rows. A set gives
/// each operation its own best sequence: the comparisons that are not its
/// own are made of [`Vectors::eq`] and [`Vectors::gt`], or of their int8
/// forms, by swapping the operands and inverting.
pub(crate) trait Vectors: Token + 'static {
    /// The int4 lanes of one group.
    type I32: Copy;
    /// The lane masks of a comparison of int4 lanes: all ones where it
    /// holds, zero elsewhere.
    type M32: Copy;
    /// The int4 lanes of a vector.
    const LANES: usize;

    /// The lanes of group `group` of a dense block.
    ///
    /// # Safety
    ///
    /// `group` must be below `64 / LANES`.
    unsafe fn load(self, values: &[i32; 64], group: usize) -> Self::I32;

    /// The lanes of group `group` of a Datum block: the low half of each
    /// Datum, as `DatumGetInt32` reads it.
    ///
    /// # Safety
    ///
    /// `group` must be below `64 / LANES`.
    unsafe fn load_datums(self, values: &[u64; 64], group: usize) -> Self::I32;

    /// `value` in every lane.
    fn splat(self, value: i32) -> Self::I32;

    /// The lanes where `a == b`.
    fn eq(self, a: Self::I32, b: Self::I32) -> Self::M32;

    /// The lanes where `a > b`.
    fn gt(self, a: Self::I32, b: Self::I32) -> Self::M32;

    /// The lanes where `a >= b`.
    #[inline(always)]
    fn ge(self, a: Self::I32, b: Self::I32) -> Self::M32 {
        self.not(self.gt(b, a))
    }

    /// The lanes where `a < b`.
    #[inline(always)]
    fn lt(self, a: Self::I32, b: Self::I32) -> Self::M32 {
        self.gt(b, a)
    }

    /// The lanes where `a <= b`.
    #[inline(always)]
    fn le(self, a: Self::I32, b: Self::I32) -> Self::M32 {
        self.ge(b, a)
    }

    /// The lanes where `mask` does not hold.
    fn not(self, mask: Self::M32) -> Self::M32;

    /// The bits of a word in the order of its rows, from the lane masks of
    /// its groups: `lanes(group)` for every group below `64 / LANES`, and
    /// for no other.
    fn pack(self, lanes: impl Fn(usize) -> Self::M32) -> u64;

    /// The rows of a Datum block whose flag is false, as bits in the order
    /// of its rows.
    fn non_null_bits(self, isnull: &[bool; 64]) -> u64;

    /// The int8 lanes of one vector.
    type I64: Copy;
    /// The lane masks of a comparison of int8 lanes.
    type M64: Copy;
    /// The int8 lanes of a vector.
    const LANES64: usize;

    /// The lanes of vector `index` of a dense int8 block.
    ///
    /// # Safety
    ///
    /// `index` must be below `64 / LANES64`.
    unsafe fn load64(self, values: &[i64; 64], index: usize) -> Self::I64;

    /// The lanes of vector `index` of a Datum block of int8 values: a
    /// Datum holds an int8 whole, as `DatumGetInt64` reads it.
    ///
    /// # Safety
    ///
    /// `index` must be below `64 / LANES64`.
    unsafe fn load_datums64(self, values: &[u64; 64], index: usize) -> Self::I64;

    /// `value` in every int8 lane.
    fn splat64(self, value: i64) -> Self::I64;

    /// The int8 lanes where `a == b`.
    fn eq64(self, a: Self::I64, b: Self::I64) -> Self::M64;

    /// The int8 lanes where `a > b`.
    fn gt64(self, a: Self::I64, b: Self::I64) -> Self::M64;

    /// The int8 lanes where `a >= b`.
    #[inline(always)]
    fn ge64(self, a: Self::I64, b: Self::I64) -> Self::M64 {
        self.not64(self.gt64(b, a))
    }

    /// The int8 lanes where `a < b`.
    #[inline(always)]
    fn lt64(self, a: Self::I64, b: Self::I64) -> Self::M64 {
        self.gt64(b, a)
    }

    /// The int8 lanes where `a <= b`.
    #[inline(always)]
    fn le64(self, a: Self::I64, b: Self::I64) -> Self::M64 {
        self.ge64(b, a)
    }

    /// The int8 lanes where `mask` does not hold.
    fn not64(self, mask: Self::M64) -> Self::M64;

    /// The bits of a word in the order of its rows, from the lane masks of
    /// its int8 vectors, `lanes(index)` for every index below `64 /
    /// LANES64` and for no other, inverted when `invert` is: a set inverts
    /// where it is cheapest, as NEON on the narrowed masks of four rows.
    fn pack64(self, invert: bool, lanes: impl Fn(usize) -> Self::M64) -> u64;
}

/// The primitives of a set this target does not run: its token is an
/// empty `enum`, so none of them can be called.
macro_rules! unreachable_vectors {
    ($set:ty) => {
        impl Vectors for $set {
            type I32 = Self;
            type M32 = Self;
            const LANES: usize = 1;

            unsafe fn load(self, _: &[i32; 64], _: usize) -> Self {
                match self {}
            }

            unsafe fn load_datums(self, _: &[u64; 64], _: usize) -> Self {
                match self {}
            }

            fn splat(self, _: i32) -> Self {
                match self {}
            }

            fn eq(self, _: Self, _: Self) -> Self {
                match self {}
            }

            fn gt(self, _: Self, _: Self) -> Self {
                match self {}
            }

            fn not(self, _: Self) -> Self {
                match self {}
            }

            fn pack(self, _: impl Fn(usize) -> Self) -> u64 {
                match self {}
            }

            fn non_null_bits(self, _: &[bool; 64]) -> u64 {
                match self {}
            }

            type I64 = Self;
            type M64 = Self;
            const LANES64: usize = 1;

            unsafe fn load64(self, _: &[i64; 64], _: usize) -> Self {
                match self {}
            }

            unsafe fn load_datums64(self, _: &[u64; 64], _: usize) -> Self {
                match self {}
            }

            fn splat64(self, _: i64) -> Self {
                match self {}
            }

            fn eq64(self, _: Self, _: Self) -> Self {
                match self {}
            }

            fn gt64(self, _: Self, _: Self) -> Self {
                match self {}
            }

            fn not64(self, _: Self) -> Self {
                match self {}
            }

            fn pack64(self, _: bool, _: impl Fn(usize) -> Self) -> u64 {
                match self {}
            }
        }
    };
}

#[cfg(not(all(target_arch = "aarch64", not(miri))))]
unreachable_vectors!(Neon);
#[cfg(not(all(target_arch = "x86_64", not(miri))))]
unreachable_vectors!(Avx2);

#[cfg(all(target_arch = "aarch64", not(miri)))]
mod aggregate;
#[cfg(all(target_arch = "aarch64", not(miri)))]
mod aggregate64;
#[cfg(all(target_arch = "aarch64", not(miri)))]
mod arith;
#[cfg(all(target_arch = "aarch64", not(miri)))]
mod arith64;
#[cfg(all(target_arch = "aarch64", not(miri)))]
mod cast;
mod filter;
mod filter64;
#[cfg(all(target_arch = "aarch64", not(miri)))]
mod hash;
#[cfg(all(target_arch = "aarch64", not(miri)))]
mod hash64;
#[cfg(all(target_arch = "aarch64", not(miri)))]
mod set;

#[cfg(all(target_arch = "aarch64", not(miri)))]
use core::arch::aarch64::{
    int32x4_t, uint8x8_t, uint32x4_t, uint64x2_t, vaddv_u8, vaddvq_u32, vandq_u8, vandq_u32,
    vceqzq_u8, vdup_n_u8, vdupq_n_u32, vget_high_s16, vget_high_u8, vget_low_s16, vget_low_s32,
    vget_low_u8, vld1_u8, vld1q_s32, vld1q_u8, vld1q_u32, vld1q_u64, vmovl_high_s32, vmovl_s8,
    vmovl_s16, vmovl_s32, vorrq_u32, vreinterpret_s8_u8, vreinterpretq_s32_u32,
    vreinterpretq_s32_u64, vreinterpretq_u32_s32, vreinterpretq_u64_s64, vtst_u8, vuzp1q_s32,
};

#[cfg(all(target_arch = "aarch64", not(miri)))]
pub use aggregate::{
    count_datum, max_datum, max_dense, min_datum, min_dense, sum_datum, sum_dense,
};
#[cfg(all(target_arch = "aarch64", not(miri)))]
pub use aggregate64::{max_datum64, max_dense64, min_datum64, min_dense64};
#[cfg(all(target_arch = "aarch64", not(miri)))]
pub use arith::{add, div, mul, rem, sub};
#[cfg(all(target_arch = "aarch64", not(miri)))]
pub use arith64::{add64, sub64};
#[cfg(all(target_arch = "aarch64", not(miri)))]
pub use cast::{widen_datum, widen_dense};
pub use filter::{compare_sides, filter_datum, filter_dense};
pub use filter64::{compare_sides64, filter_datum64, filter_dense64};
#[cfg(all(target_arch = "aarch64", not(miri)))]
pub use hash::{combine, combine_nulls, hash, hash_nulls};
#[cfg(all(target_arch = "aarch64", not(miri)))]
pub use hash64::{combine_nulls64, combine64, hash_nulls64, hash64};
#[cfg(all(target_arch = "aarch64", not(miri)))]
pub use set::{set_datum, set_datum64, set_dense, set_dense64};

/// Bit weights of the four lanes of each group in a 16-row quarter.
#[cfg(all(target_arch = "aarch64", not(miri)))]
const LANE_WEIGHTS: [[u32; 4]; 4] = [
    [1, 1 << 1, 1 << 2, 1 << 3],
    [1 << 4, 1 << 5, 1 << 6, 1 << 7],
    [1 << 8, 1 << 9, 1 << 10, 1 << 11],
    [1 << 12, 1 << 13, 1 << 14, 1 << 15],
];
/// Bit weights of the bytes of two 8-row halves.
#[cfg(all(target_arch = "aarch64", not(miri)))]
const BYTE_WEIGHTS: [u8; 16] = [1, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128];

/// Rows of a Datum block whose flag is false, as bits in row order.
#[cfg(all(target_arch = "aarch64", not(miri)))]
#[inline]
pub fn non_null_bits(_: Neon, isnull: &[bool; 64]) -> u64 {
    // SAFETY: the token proves that this processor runs NEON.
    unsafe { non_null_lanes(isnull) }
}

#[cfg(all(target_arch = "aarch64", not(miri)))]
#[inline]
#[target_feature(enable = "neon")]
fn non_null_lanes(isnull: &[bool; 64]) -> u64 {
    // SAFETY: sixteen weights.
    let weights = unsafe { vld1q_u8(BYTE_WEIGHTS.as_ptr()) };
    let mut bits = 0;
    for quarter in 0..4 {
        // SAFETY: a bool is one byte holding 0 or 1, and the sixteen bytes
        // read end within the array.
        let flags = unsafe { vld1q_u8(isnull.as_ptr().cast::<u8>().add(quarter * 16)) };
        let present = vandq_u8(vceqzq_u8(flags), weights);
        let low = u64::from(vaddv_u8(vget_low_u8(present)));
        let high = u64::from(vaddv_u8(vget_high_u8(present)));
        bits |= (low | high << 8) << (quarter * 16);
    }
    bits
}

/// The bits of the 64 rows of a word from its 16 groups of four lane
/// masks, `lanes(group)` for each. A passing lane is all ones. Weighting
/// each lane by its bit and adding the four groups of a quarter gives the
/// quarter's 16 bits without a per-row shift; four quarters make the word.
#[cfg(all(target_arch = "aarch64", not(miri)))]
#[inline]
#[target_feature(enable = "neon")]
fn pack_lanes(lanes: impl Fn(usize) -> uint32x4_t) -> u64 {
    // SAFETY: every weight row has exactly four lanes.
    let weights = LANE_WEIGHTS.map(|row| unsafe { vld1q_u32(row.as_ptr()) });
    let mut bits = 0;
    for quarter in 0..4 {
        let mut passing = vdupq_n_u32(0);
        for (group, weight) in weights.iter().enumerate() {
            passing = vorrq_u32(passing, vandq_u32(lanes(quarter * 4 + group), *weight));
        }
        bits |= u64::from(vaddvq_u32(passing)) << (quarter * 16);
    }
    bits
}

/// The bit weights of eight rows, one per byte lane.
#[cfg(all(target_arch = "aarch64", not(miri)))]
#[inline]
#[target_feature(enable = "neon")]
fn byte_weights() -> uint8x8_t {
    // SAFETY: eight weights.
    unsafe { vld1_u8(BYTE_WEIGHTS.as_ptr()) }
}

/// Eight mask bits as two groups of four lane masks: a set bit becomes an
/// all-ones lane. The byte is broadcast, tested against each lane's weight
/// and sign-extended twice.
#[cfg(all(target_arch = "aarch64", not(miri)))]
#[inline]
#[target_feature(enable = "neon")]
fn lane_masks(bits: u8, weights: uint8x8_t) -> (uint32x4_t, uint32x4_t) {
    let bytes = vreinterpret_s8_u8(vtst_u8(vdup_n_u8(bits), weights));
    let halves = vmovl_s8(bytes);
    (
        vreinterpretq_u32_s32(vmovl_s16(vget_low_s16(halves))),
        vreinterpretq_u32_s32(vmovl_s16(vget_high_s16(halves))),
    )
}

/// A loader of the four int4 values of each group of a dense block.
#[cfg(all(target_arch = "aarch64", not(miri)))]
#[inline]
#[target_feature(enable = "neon")]
fn dense(values: &[i32; 64]) -> impl Fn(usize) -> int32x4_t + '_ {
    // SAFETY: `group` is below 16, so the four lanes read end within the array.
    move |group| unsafe { vld1q_s32(values.as_ptr().add(group * 4)) }
}

/// A loader of the four int4 values of each group of a Datum block.
#[cfg(all(target_arch = "aarch64", not(miri)))]
#[inline]
#[target_feature(enable = "neon")]
fn datum(values: &[u64; 64]) -> impl Fn(usize) -> int32x4_t + '_ {
    // SAFETY: `group` is below 16.
    move |group| unsafe { load_datums(values, group) }
}

/// Four int4 values from four Datums: the low half of each, kept by uzp1.
///
/// # Safety
///
/// `group` must be below 16 so that the four Datums read end within the array.
#[cfg(all(target_arch = "aarch64", not(miri)))]
#[inline]
#[target_feature(enable = "neon")]
unsafe fn load_datums(values: &[u64; 64], group: usize) -> int32x4_t {
    // SAFETY: the caller keeps `group` below 16.
    let (first, second) = unsafe {
        let start = values.as_ptr().add(group * 4);
        (vld1q_u64(start), vld1q_u64(start.add(2)))
    };
    vuzp1q_s32(vreinterpretq_s32_u64(first), vreinterpretq_s32_u64(second))
}

/// Four rows of lane masks widened to two pairs: an all-ones 32-bit lane
/// sign-extends to an all-ones 64-bit one.
#[cfg(all(target_arch = "aarch64", not(miri)))]
#[inline]
#[target_feature(enable = "neon")]
fn widen(rows: uint32x4_t) -> (uint64x2_t, uint64x2_t) {
    let rows = vreinterpretq_s32_u32(rows);
    (
        vreinterpretq_u64_s64(vmovl_s32(vget_low_s32(rows))),
        vreinterpretq_u64_s64(vmovl_high_s32(rows)),
    )
}
