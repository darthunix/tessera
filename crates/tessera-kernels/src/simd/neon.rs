//! The primitives of NEON: [`Vectors`] for [`Neon`], four int4 lanes to a
//! vector and 16 groups to a word. Each is an intrinsic, or a helper of
//! this module made of them, called in an `unsafe` block whose reason is
//! the token: a value of `Neon` exists only where the processor runs NEON.
//!
//! Every function of AArch64 carries NEON, so the primitives are
//! `#[inline]`, as the intrinsics are: `#[inline(always)]` inlined them
//! earlier than the intrinsics had been, and the NEON code of the kernels
//! moved here changed its loads and spills.

use core::arch::aarch64::{
    int32x4_t, uint32x4_t, vceqq_s32, vcgeq_s32, vcgtq_s32, vcleq_s32, vcltq_s32, vdupq_n_s32,
    vld1q_s32, vmvnq_u32,
};

use super::{Neon, Vectors, load_datums, non_null_lanes, pack_lanes};

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
}
