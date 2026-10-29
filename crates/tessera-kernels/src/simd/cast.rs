//! Whole-word widening of int4 values into int8 lanes, written once over
//! the primitives of [`Vectors`].
//!
//! Every lane of the word is widened and stored, NULL and unselected
//! lanes included: their placeholders are what the output contract allows,
//! and the caller's mask says which lanes hold a value.

// Under Miri no set runs, so the tokens have no values, and the code that
// the functions compiled for a set run on theirs cannot be reached.
#![cfg_attr(miri, allow(unreachable_code, unused_variables))]

use std::mem::MaybeUninit;

use super::{Neon, Simd, Vectors};

/// The int4 values of a dense block widened into `out`.
#[inline]
pub fn widen_dense(neon: Neon, values: &[i32; 64], out: &mut [MaybeUninit<i64>; 64]) {
    // SAFETY: the token proves that this processor runs NEON, the vector
    // set of the target where it has values, which `widen_dense_lanes` is
    // compiled for.
    unsafe { widen_dense_lanes(neon.simd(), values, out) }
}

/// The int4 values of a Datum block widened into `out`.
#[inline]
pub fn widen_datum(neon: Neon, values: &[u64; 64], out: &mut [MaybeUninit<i64>; 64]) {
    // SAFETY: as in widen_dense, for `widen_datum_lanes`.
    unsafe { widen_datum_lanes(neon.simd(), values, out) }
}

// The functions compiled for the vector set of the target, into which the
// kernel below is inlined; the kernel carries its feature and is
// `#[inline]`, as in `arith.rs`.

#[cfg_attr(
    all(target_arch = "aarch64", not(miri)),
    target_feature(enable = "neon")
)]
#[cfg_attr(
    all(target_arch = "x86_64", not(miri)),
    target_feature(enable = "avx2")
)]
unsafe fn widen_dense_lanes(simd: Simd, values: &[i32; 64], out: &mut [MaybeUninit<i64>; 64]) {
    // SAFETY: `store` asks for the groups of a word only.
    let load = |group: usize| unsafe { simd.load(values, group) };
    store(simd, out, load);
}

#[cfg_attr(
    all(target_arch = "aarch64", not(miri)),
    target_feature(enable = "neon")
)]
#[cfg_attr(
    all(target_arch = "x86_64", not(miri)),
    target_feature(enable = "avx2")
)]
unsafe fn widen_datum_lanes(simd: Simd, values: &[u64; 64], out: &mut [MaybeUninit<i64>; 64]) {
    // SAFETY: as in widen_dense_lanes.
    let load = |group: usize| unsafe { simd.load_datums(values, group) };
    store(simd, out, load);
}

/// Every group of int4 lanes sign-extended into two vectors of int8
/// lanes, stored in place.
#[inline]
#[cfg_attr(
    all(target_arch = "aarch64", not(miri)),
    target_feature(enable = "neon")
)]
#[cfg_attr(
    all(target_arch = "x86_64", not(miri)),
    target_feature(enable = "avx2")
)]
fn store<V: Vectors>(v: V, out: &mut [MaybeUninit<i64>; 64], load: impl Fn(usize) -> V::I32) {
    let base = out.as_mut_ptr().cast::<i64>();
    for group in 0..64 / V::LANES {
        let (low, high) = v.widen64(load(group));
        // SAFETY: `group` is below `64 / LANES`, and a group's lanes are
        // two vectors of int8 lanes, so the lanes written end within the
        // array; MaybeUninit<i64> has i64's layout.
        unsafe {
            v.store64(base.add(group * V::LANES), low);
            v.store64(base.add(group * V::LANES + V::LANES64), high);
        }
    }
}
