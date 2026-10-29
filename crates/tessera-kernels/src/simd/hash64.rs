//! Whole-word int8 key hashes: every lane folded to 32 bits as
//! PostgreSQL's `hashint8` folds it, then hashed and combined as the int4
//! lanes of [`super::hash`].
//!
//! Dense int8 storage and Datum storage are the same eight bytes per row,
//! so one loader serves both: a group's keys are two vectors of int8
//! lanes, split into their low and high halves, and the fold is
//! `low ^ high ^ (high >> 31)` with an arithmetic shift, the high half
//! inverted for negative keys.

// Under Miri no set runs, so the tokens have no values, and the code that
// the functions compiled for a set run on theirs cannot be reached.
#![cfg_attr(miri, allow(unreachable_code, unused_variables))]

use super::hash::{combined, groups};
use super::{Simd, Vectors};
use crate::int64::Side;

/// The hash of every lane's key into `out`.
#[inline]
pub fn hash64(simd: Simd, keys: Side<'_>, out: &mut [u32; 64]) {
    // SAFETY: the token proves that this processor runs the vector set of
    // the target, which `hash_lanes` is compiled for.
    unsafe { hash_lanes(simd, keys, None, out) }
}

/// As [`hash64`], with NULL lanes hashing the group key.
#[inline]
pub fn hash_nulls64(simd: Simd, keys: Side<'_>, non_nulls: u64, out: &mut [u32; 64]) {
    // SAFETY: as in hash64.
    unsafe { hash_lanes(simd, keys, Some(non_nulls), out) }
}

/// Every lane's key hash folded into its previous hash in `out`.
#[inline]
pub fn combine64(simd: Simd, keys: Side<'_>, out: &mut [u32; 64]) {
    // SAFETY: as in hash64, for `combine_lanes`.
    unsafe { combine_lanes(simd, keys, None, out) }
}

/// As [`combine64`], with NULL lanes folding in the group key's hash.
#[inline]
pub fn combine_nulls64(simd: Simd, keys: Side<'_>, non_nulls: u64, out: &mut [u32; 64]) {
    // SAFETY: as in hash64, for `combine_lanes`.
    unsafe { combine_lanes(simd, keys, Some(non_nulls), out) }
}

// The functions compiled for the vector set of the target, into which the
// kernels are inlined, as in `hash.rs`.

#[cfg_attr(
    all(target_arch = "aarch64", not(miri)),
    target_feature(enable = "neon")
)]
#[cfg_attr(
    all(target_arch = "x86_64", not(miri)),
    target_feature(enable = "avx2")
)]
unsafe fn hash_lanes(simd: Simd, keys: Side<'_>, non_nulls: Option<u64>, out: &mut [u32; 64]) {
    groups(
        simd,
        folded(simd, words(keys)),
        non_nulls,
        out,
        |key_hash, _| key_hash,
    )
}

#[cfg_attr(
    all(target_arch = "aarch64", not(miri)),
    target_feature(enable = "neon")
)]
#[cfg_attr(
    all(target_arch = "x86_64", not(miri)),
    target_feature(enable = "avx2")
)]
unsafe fn combine_lanes(simd: Simd, keys: Side<'_>, non_nulls: Option<u64>, out: &mut [u32; 64]) {
    let golden = simd.splat(0x9e37_79b9_u32 as i32);
    groups(
        simd,
        folded(simd, words(keys)),
        non_nulls,
        out,
        |key_hash, previous| combined(simd, golden, key_hash, previous),
    )
}

/// The 64 eight-byte words of a block, dense or Datum alike.
#[inline(always)]
fn words(keys: Side<'_>) -> &[u64; 64] {
    match keys {
        // SAFETY: `[i64; 64]` and `[u64; 64]` have the same size and
        // alignment, and every bit pattern is valid for both.
        Side::Dense(values) => unsafe { &*values.as_ptr().cast::<[u64; 64]>() },
        Side::Datum(values) => values,
        Side::Scalar(_) => unreachable!("a scalar key"),
    }
}

/// A loader of the folded keys of each group of lanes.
#[inline]
#[cfg_attr(
    all(target_arch = "aarch64", not(miri)),
    target_feature(enable = "neon")
)]
#[cfg_attr(
    all(target_arch = "x86_64", not(miri)),
    target_feature(enable = "avx2")
)]
fn folded<V: Vectors>(v: V, words: &[u64; 64]) -> impl Fn(usize) -> V::I32 + '_ {
    const { assert!(V::LANES == 2 * V::LANES64) }
    move |group| {
        // SAFETY: `group` is below `64 / LANES`, so both vectors are below
        // `64 / LANES64`.
        let (low, high) = unsafe {
            v.halves(
                v.load_datums64(words, group * 2),
                v.load_datums64(words, group * 2 + 1),
            )
        };
        v.xor(low, v.xor(high, v.sar::<31>(high)))
    }
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use super::{combine_nulls64, combine64, hash_nulls64, hash64};
    use crate::int64::{Side, fold, hash_combine, murmurhash32};
    use crate::simd::{Simd, Token};

    fn keys() -> [i64; 64] {
        core::array::from_fn(|lane| {
            let lane = lane as i64;
            match lane % 4 {
                0 => lane * 1_000_003 - 20,
                1 => -(lane << 37) - 1,
                2 => i64::from(i32::MIN) + lane,
                _ => i64::MAX - lane * 7,
            }
        })
    }

    #[test]
    fn lanes_match_the_row_formulas() {
        let keys = keys();
        let words = keys.map(|key| key as u64);
        let non_nulls = 0x5555_aaaa_0f0f_f0f0_u64;
        let Some(simd) = Simd::get() else {
            return;
        };
        for side in [Side::Dense(&keys), Side::Datum(&words)] {
            let mut out = [0; 64];
            hash64(simd, side, &mut out);
            assert_eq!(out, keys.map(|key| murmurhash32(fold(key))));
            let mut grouped = [0; 64];
            hash_nulls64(simd, side, non_nulls, &mut grouped);
            let mut previous = [0x1234_5678; 64];
            combine64(simd, side, &mut previous);
            let mut previous_nulls = [0x1234_5678; 64];
            combine_nulls64(simd, side, non_nulls, &mut previous_nulls);
            for lane in 0..64 {
                let present = non_nulls >> lane & 1 == 1;
                let key = if present {
                    fold(keys[lane])
                } else {
                    0x9e37_79b9
                };
                assert_eq!(grouped[lane], murmurhash32(key), "lane {lane}");
                assert_eq!(
                    previous[lane],
                    hash_combine(0x1234_5678, murmurhash32(fold(keys[lane]))),
                    "lane {lane}"
                );
                assert_eq!(
                    previous_nulls[lane],
                    hash_combine(0x1234_5678, murmurhash32(key)),
                    "lane {lane}"
                );
            }
        }
    }
}
