//! Whole-word key hashes: `murmurhash32` of every lane, optionally folded
//! into the lanes' previous hashes with `hash_combine`, written once over
//! the primitives of [`Vectors`].
//!
//! Every lane is computed and stored; nothing can fail. With a non-NULL
//! mask, NULL lanes take the group key `0x9e3779b9` as their value, which
//! gives them the group policy's fixed hash; without one, their lanes hold
//! meaningless values that the caller's valid mask excludes. A key's 32
//! bits are held in int4 lanes: their products, sums and xors are those of
//! unsigned lanes, and a shift right shifts zeros in.

// Under Miri no set runs, so the tokens have no values, and the code that
// the functions compiled for a set run on theirs cannot be reached.
#![cfg_attr(miri, allow(unreachable_code, unused_variables))]

use super::{Neon, Simd, Vectors};
use crate::int32::Side;
use crate::int32::hash::NULL_KEY;

/// The hash of every lane's key into `out`.
#[inline]
pub fn hash(neon: Neon, keys: Side<'_>, out: &mut [u32; 64]) {
    // SAFETY: the token proves that this processor runs NEON, the vector
    // set of the target where it has values, which `hash_lanes` is
    // compiled for.
    unsafe { hash_lanes(neon.simd(), keys, None, out) }
}

/// As [`hash`], with NULL lanes hashing the group key.
#[inline]
pub fn hash_nulls(neon: Neon, keys: Side<'_>, non_nulls: u64, out: &mut [u32; 64]) {
    // SAFETY: as in hash.
    unsafe { hash_lanes(neon.simd(), keys, Some(non_nulls), out) }
}

/// Every lane's key hash folded into its previous hash in `out`.
#[inline]
pub fn combine(neon: Neon, keys: Side<'_>, out: &mut [u32; 64]) {
    // SAFETY: as in hash, for `combine_lanes`.
    unsafe { combine_lanes(neon.simd(), keys, None, out) }
}

/// As [`combine`], with NULL lanes folding in the group key's hash.
#[inline]
pub fn combine_nulls(neon: Neon, keys: Side<'_>, non_nulls: u64, out: &mut [u32; 64]) {
    // SAFETY: as in hash, for `combine_lanes`.
    unsafe { combine_lanes(neon.simd(), keys, Some(non_nulls), out) }
}

// The functions compiled for the vector set of the target, into which the
// kernels below are inlined; the kernels carry its feature and are
// `#[inline]`, as in `arith.rs`.

#[cfg_attr(
    all(target_arch = "aarch64", not(miri)),
    target_feature(enable = "neon")
)]
#[cfg_attr(
    all(target_arch = "x86_64", not(miri)),
    target_feature(enable = "avx2")
)]
unsafe fn hash_lanes(simd: Simd, keys: Side<'_>, non_nulls: Option<u64>, out: &mut [u32; 64]) {
    transform(simd, keys, non_nulls, out, |key_hash, _| key_hash)
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
    transform(simd, keys, non_nulls, out, |key_hash, previous| {
        combined(simd, golden, key_hash, previous)
    })
}

/// `hash_combine` in every lane, `golden` holding `0x9e3779b9`:
/// `a ^ (b + 0x9e3779b9 + (a << 6) + (a >> 2))`, with `a` the previous
/// hash and `b` the key's.
#[inline]
#[cfg_attr(
    all(target_arch = "aarch64", not(miri)),
    target_feature(enable = "neon")
)]
#[cfg_attr(
    all(target_arch = "x86_64", not(miri)),
    target_feature(enable = "avx2")
)]
pub(super) fn combined<V: Vectors>(
    v: V,
    golden: V::I32,
    key_hash: V::I32,
    previous: V::I32,
) -> V::I32 {
    let mut t = v.wrapping_add(key_hash, golden);
    t = v.wrapping_add(t, v.shl::<6>(previous));
    t = v.wrapping_add(t, v.shr::<2>(previous));
    v.xor(previous, t)
}

/// murmurhash32 in every lane.
#[inline]
#[cfg_attr(
    all(target_arch = "aarch64", not(miri)),
    target_feature(enable = "neon")
)]
#[cfg_attr(
    all(target_arch = "x86_64", not(miri)),
    target_feature(enable = "avx2")
)]
fn murmur<V: Vectors>(v: V, mut h: V::I32, first: V::I32, second: V::I32) -> V::I32 {
    h = v.xor(h, v.shr::<16>(h));
    h = v.wrapping_mul(h, first);
    h = v.xor(h, v.shr::<13>(h));
    h = v.wrapping_mul(h, second);
    v.xor(h, v.shr::<16>(h))
}

/// Choose the loader for the storage once per word.
#[inline]
#[cfg_attr(
    all(target_arch = "aarch64", not(miri)),
    target_feature(enable = "neon")
)]
#[cfg_attr(
    all(target_arch = "x86_64", not(miri)),
    target_feature(enable = "avx2")
)]
fn transform<V: Vectors>(
    v: V,
    keys: Side<'_>,
    non_nulls: Option<u64>,
    out: &mut [u32; 64],
    f: impl Fn(V::I32, V::I32) -> V::I32,
) {
    match keys {
        Side::Dense(values) => {
            // SAFETY: `groups` asks for the groups of a word only.
            let load = |group: usize| unsafe { v.load(values, group) };
            groups(v, load, non_nulls, out, f)
        }
        Side::Datum(values) => {
            // SAFETY: as above.
            let load = |group: usize| unsafe { v.load_datums(values, group) };
            groups(v, load, non_nulls, out, f)
        }
        Side::Scalar(_) => unreachable!("a scalar key"),
    }
}

/// Every group of lanes: the key (the group key in NULL lanes when a mask
/// is given), its hash, `f` of that and the previous hash, stored.
#[inline]
#[cfg_attr(
    all(target_arch = "aarch64", not(miri)),
    target_feature(enable = "neon")
)]
#[cfg_attr(
    all(target_arch = "x86_64", not(miri)),
    target_feature(enable = "avx2")
)]
pub(super) fn groups<V: Vectors>(
    v: V,
    load: impl Fn(usize) -> V::I32,
    non_nulls: Option<u64>,
    out: &mut [u32; 64],
    f: impl Fn(V::I32, V::I32) -> V::I32,
) {
    let first = v.splat(0x85eb_ca6b_u32 as i32);
    let second = v.splat(0xc2b2_ae35_u32 as i32);
    let base = out.as_mut_ptr().cast::<i32>();
    let one = |group: usize, key: V::I32| {
        // SAFETY: `group` is below `64 / LANES`, so the lanes read and
        // written end within the array, whose u32 values have the layout
        // of i32 ones; the array read is borrowed for the load alone.
        unsafe {
            let previous = v.load(&*base.cast::<[i32; 64]>(), group);
            v.store(
                base.add(group * V::LANES),
                f(murmur(v, key, first, second), previous),
            );
        }
    };
    match non_nulls {
        None => {
            for group in 0..64 / V::LANES {
                one(group, load(group));
            }
        }
        Some(bits) => {
            let null_key = v.splat(NULL_KEY as i32);
            for byte in 0..8 {
                let masks = v.lane_masks((bits >> (byte * 8)) as u8);
                for (group, lanes) in (byte * (8 / V::LANES)..).zip(masks) {
                    one(group, v.select(lanes, load(group), null_key));
                }
            }
        }
    }
}

// The vector code runs only on NEON until its hooks take the set of the
// target.
#[cfg(test)]
#[cfg(all(target_arch = "aarch64", not(miri)))]
mod tests {
    use anyhow::Result;
    use proptest::collection::vec;
    use proptest::prelude::*;
    use tessera_testing::{integer, property};

    use super::{combine, combine_nulls, hash, hash_nulls};
    use crate::int32::{Side, hash_combine, murmurhash32};
    use crate::simd::{Neon, Token};

    /// Any word of keys leaning to the edges, any NULL lanes and any
    /// previous hashes.
    #[test]
    fn lanes_match_the_scalar_functions_on_both_storages() {
        let cases = (
            vec(integer::<i32>(), 64),
            any::<u64>(),
            vec(any::<u32>(), 64),
        );
        property(cases, |(keys, non_nulls, previous)| -> Result<()> {
            lanes_match(
                keys.try_into().unwrap(),
                non_nulls,
                previous.try_into().unwrap(),
            );
            Ok(())
        });
    }

    fn lanes_match(dense: [i32; 64], non_nulls: u64, previous: [u32; 64]) {
        let neon = Neon::get().unwrap();
        let datums = dense.map(|key| i64::from(key) as u64);
        let expected_hash = dense.map(|key| murmurhash32(key as u32));
        let expected_hash_nulls: [u32; 64] = std::array::from_fn(|lane| {
            if non_nulls & (1 << lane) != 0 {
                murmurhash32(dense[lane] as u32)
            } else {
                murmurhash32(0x9e37_79b9)
            }
        });
        let expected_combine: [u32; 64] =
            std::array::from_fn(|lane| hash_combine(previous[lane], expected_hash[lane]));
        let expected_combine_nulls: [u32; 64] =
            std::array::from_fn(|lane| hash_combine(previous[lane], expected_hash_nulls[lane]));
        for keys in [Side::Dense(&dense), Side::Datum(&datums)] {
            let mut out = [0; 64];
            hash(neon, keys, &mut out);
            assert_eq!(out, expected_hash);
            hash_nulls(neon, keys, non_nulls, &mut out);
            assert_eq!(out, expected_hash_nulls);
            out = previous;
            combine(neon, keys, &mut out);
            assert_eq!(out, expected_combine);
            out = previous;
            combine_nulls(neon, keys, non_nulls, &mut out);
            assert_eq!(out, expected_combine_nulls);
        }
    }
}
