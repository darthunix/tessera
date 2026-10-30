//! Whole-word membership of integer values in a few keys: every lane is
//! compared with every key and the equalities or-ed, so that a word costs
//! the same whatever its values. The lanes of a pass stay in registers
//! while the keys go by, each key broadcast once a pass; a pass is packed
//! into bits a quarter of the word at a time.
//!
//! A pass is 32 rows of int4 values or 16 of int8 ones on every set: eight
//! vectors on NEON and four on AVX2, whose vectors are twice as wide and
//! whose registers half as many.

// Under Miri no set runs, so the tokens have no values, and the code that
// the functions compiled for a set run on theirs cannot be reached.
#![cfg_attr(miri, allow(unreachable_code, unused_variables))]

use core::array;

use super::{Neon, Simd, Vectors};

/// Rows of a pass of int4 values.
const PASS: usize = 32;

/// The most vectors a pass holds, on the set with the narrowest ones.
const PASS_VECTORS: usize = 8;

/// Rows of a dense block whose value is one of `keys`, as bits in row
/// order. NULL rows compare like any other and are masked by the caller.
#[inline]
pub fn set_dense(neon: Neon, values: &[i32; 64], keys: &[i32]) -> u64 {
    // SAFETY: the token proves that this processor runs NEON, the vector
    // set of the target where it has values, which `dense_lanes` is
    // compiled for.
    unsafe { dense_lanes(neon.simd(), values, keys) }
}

/// Rows of a Datum block whose int4 value is one of `keys`, NULL rows
/// included as above.
#[inline]
pub fn set_datum(neon: Neon, values: &[u64; 64], keys: &[i32]) -> u64 {
    // SAFETY: as in set_dense, for `datum_lanes`.
    unsafe { datum_lanes(neon.simd(), values, keys) }
}

/// Rows of a dense block of int8 values that are one of `keys`, NULL rows
/// included as above.
#[inline]
pub fn set_dense64(neon: Neon, values: &[i64; 64], keys: &[i64]) -> u64 {
    // SAFETY: as in set_dense, for `lanes64`.
    unsafe { lanes64(neon.simd(), values, keys) }
}

/// Rows of a Datum block whose int8 value is one of `keys`, NULL rows
/// included as above.
#[inline]
pub fn set_datum64(neon: Neon, values: &[u64; 64], keys: &[i64]) -> u64 {
    // SAFETY: `[u64; 64]` and `[i64; 64]` have the same size and alignment,
    // and every bit pattern is valid for both: a Datum holding an int8 is
    // its bits.
    let values = unsafe { &*values.as_ptr().cast::<[i64; 64]>() };
    // SAFETY: as in set_dense, for `lanes64`.
    unsafe { lanes64(neon.simd(), values, keys) }
}

// The functions compiled for the vector set of the target, into which the
// kernels below are inlined; the kernels carry its feature and are
// `#[inline]`, as in `arith.rs`. The functions are `#[inline]` too, as the
// NEON code of the sets was: on AArch64, whose target carries NEON, they
// are inlined into the driver, and on x86-64 they stay its calls, since
// the driver is compiled for no set.

#[inline]
#[cfg_attr(
    all(target_arch = "aarch64", not(miri)),
    target_feature(enable = "neon")
)]
#[cfg_attr(
    all(target_arch = "x86_64", not(miri)),
    target_feature(enable = "avx2")
)]
unsafe fn dense_lanes(simd: Simd, values: &[i32; 64], keys: &[i32]) -> u64 {
    // SAFETY: `set32` asks for the groups of a word only.
    set32(simd, |group| unsafe { simd.load(values, group) }, keys)
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
unsafe fn datum_lanes(simd: Simd, values: &[u64; 64], keys: &[i32]) -> u64 {
    // SAFETY: as in `dense_lanes`.
    set32(
        simd,
        |group| unsafe { simd.load_datums(values, group) },
        keys,
    )
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
unsafe fn lanes64(simd: Simd, values: &[i64; 64], keys: &[i64]) -> u64 {
    set64(simd, values, keys)
}

/// Two passes of int4 lanes, each packed as two quarters.
#[inline]
#[cfg_attr(
    all(target_arch = "aarch64", not(miri)),
    target_feature(enable = "neon")
)]
#[cfg_attr(
    all(target_arch = "x86_64", not(miri)),
    target_feature(enable = "avx2")
)]
fn set32<V: Vectors>(v: V, load: impl Fn(usize) -> V::I32, keys: &[i32]) -> u64 {
    let groups = PASS / V::LANES;
    let quarter = groups / 2;
    let mut bits = 0;
    for pass in 0..64 / PASS {
        // A set with fewer groups to a pass fills the rest with copies,
        // which no key is compared with.
        let values: [V::I32; PASS_VECTORS] =
            array::from_fn(|group| load(pass * groups + group % groups));
        let mut equal = [v.none(); PASS_VECTORS];
        for &key in keys {
            let key = v.splat(key);
            for (equal, values) in equal.iter_mut().zip(values).take(groups) {
                *equal = v.or(*equal, v.eq(values, key));
            }
        }
        bits |= v.pack_quarter(|group| equal[group]) << (pass * PASS);
        bits |= v.pack_quarter(|group| equal[quarter + group]) << (pass * PASS + 16);
    }
    bits
}

/// Four passes of int8 lanes, each a quarter of the word.
#[inline]
#[cfg_attr(
    all(target_arch = "aarch64", not(miri)),
    target_feature(enable = "neon")
)]
#[cfg_attr(
    all(target_arch = "x86_64", not(miri)),
    target_feature(enable = "avx2")
)]
fn set64<V: Vectors>(v: V, values: &[i64; 64], keys: &[i64]) -> u64 {
    let vectors = 16 / V::LANES64;
    let mut bits = 0;
    for quarter in 0..4 {
        // SAFETY: `quarter` is below 4 and `index % vectors` below
        // `vectors`, so every vector read is one of the word's; the copies
        // are as in `set32`.
        let pairs: [V::I64; PASS_VECTORS] = array::from_fn(|index| unsafe {
            v.load64(values, quarter * vectors + index % vectors)
        });
        let mut equal = [v.none64(); PASS_VECTORS];
        for &key in keys {
            let key = v.splat64(key);
            for (equal, pairs) in equal.iter_mut().zip(pairs).take(vectors) {
                *equal = v.or64(*equal, v.eq64(pairs, key));
            }
        }
        bits |= v.pack_quarter64(|index| equal[index]) << (quarter * 16);
    }
    bits
}
