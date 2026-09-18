//! Whole-word key hashes: `murmurhash32` of every lane, optionally folded
//! into the lanes' previous hashes with `hash_combine`.
//!
//! Every lane is computed and stored; nothing can fail. With a non-NULL
//! mask, NULL lanes take the group key `0x9e3779b9` as their value, which
//! gives them the group policy's fixed hash; without one, their lanes hold
//! meaningless values that the caller's valid mask excludes.

use core::arch::aarch64::{
    uint32x4_t, vaddq_u32, vbslq_u32, vdupq_n_u32, veorq_u32, vld1q_u32, vmulq_u32,
    vreinterpretq_u32_s32, vshlq_n_u32, vshrq_n_u32, vst1q_u32,
};

use super::{byte_weights, datum, dense, lane_masks};
use crate::int32::Side;
use crate::int32::hash::NULL_KEY;

/// The hash of every lane's key into `out`.
#[inline]
pub fn hash(keys: Side<'_>, out: &mut [u32; 64]) {
    const { assert!(cfg!(target_feature = "neon")) }
    // SAFETY: NEON is enabled for this compilation (asserted above).
    unsafe { hash_lanes(keys, None, out) }
}

/// As [`hash`], with NULL lanes hashing the group key.
#[inline]
pub fn hash_nulls(keys: Side<'_>, non_nulls: u64, out: &mut [u32; 64]) {
    const { assert!(cfg!(target_feature = "neon")) }
    // SAFETY: as in hash.
    unsafe { hash_lanes(keys, Some(non_nulls), out) }
}

/// Every lane's key hash folded into its previous hash in `out`.
#[inline]
pub fn combine(keys: Side<'_>, out: &mut [u32; 64]) {
    const { assert!(cfg!(target_feature = "neon")) }
    // SAFETY: as in hash.
    unsafe { combine_lanes(keys, None, out) }
}

/// As [`combine`], with NULL lanes folding in the group key's hash.
#[inline]
pub fn combine_nulls(keys: Side<'_>, non_nulls: u64, out: &mut [u32; 64]) {
    const { assert!(cfg!(target_feature = "neon")) }
    // SAFETY: as in hash.
    unsafe { combine_lanes(keys, Some(non_nulls), out) }
}

#[target_feature(enable = "neon")]
fn hash_lanes(keys: Side<'_>, non_nulls: Option<u64>, out: &mut [u32; 64]) {
    transform(keys, non_nulls, out, |key_hash, _| key_hash)
}

#[target_feature(enable = "neon")]
fn combine_lanes(keys: Side<'_>, non_nulls: Option<u64>, out: &mut [u32; 64]) {
    let golden = vdupq_n_u32(0x9e37_79b9);
    transform(keys, non_nulls, out, |key_hash, previous| {
        // a ^ (b + 0x9e3779b9 + (a << 6) + (a >> 2)), as hash_combine.
        let mut t = vaddq_u32(key_hash, golden);
        t = vaddq_u32(t, vshlq_n_u32::<6>(previous));
        t = vaddq_u32(t, vshrq_n_u32::<2>(previous));
        veorq_u32(previous, t)
    })
}

/// murmurhash32 on four lanes.
#[inline]
#[target_feature(enable = "neon")]
fn murmur(mut h: uint32x4_t, first: uint32x4_t, second: uint32x4_t) -> uint32x4_t {
    h = veorq_u32(h, vshrq_n_u32::<16>(h));
    h = vmulq_u32(h, first);
    h = veorq_u32(h, vshrq_n_u32::<13>(h));
    h = vmulq_u32(h, second);
    veorq_u32(h, vshrq_n_u32::<16>(h))
}

/// Choose the loader for the storage once per word.
#[inline]
#[target_feature(enable = "neon")]
fn transform(
    keys: Side<'_>,
    non_nulls: Option<u64>,
    out: &mut [u32; 64],
    f: impl Fn(uint32x4_t, uint32x4_t) -> uint32x4_t,
) {
    match keys {
        Side::Dense(values) => groups(dense(values), non_nulls, out, f),
        Side::Datum(values) => groups(datum(values), non_nulls, out, f),
        Side::Scalar(_) => unreachable!("a scalar key"),
    }
}

/// Every group of four lanes: the key (the group key in NULL lanes when a
/// mask is given), its hash, `f` of that and the previous hash, stored.
#[inline]
#[target_feature(enable = "neon")]
fn groups(
    load: impl Fn(usize) -> core::arch::aarch64::int32x4_t,
    non_nulls: Option<u64>,
    out: &mut [u32; 64],
    f: impl Fn(uint32x4_t, uint32x4_t) -> uint32x4_t,
) {
    let first = vdupq_n_u32(0x85eb_ca6b);
    let second = vdupq_n_u32(0xc2b2_ae35);
    let base = out.as_mut_ptr();
    let one = |group: usize, key: uint32x4_t| {
        // SAFETY: `group` is below 16, so the four lanes read and written
        // end within the array.
        unsafe {
            let previous = vld1q_u32(base.add(group * 4));
            vst1q_u32(base.add(group * 4), f(murmur(key, first, second), previous));
        }
    };
    match non_nulls {
        None => {
            for group in 0..16 {
                one(group, vreinterpretq_u32_s32(load(group)));
            }
        }
        Some(bits) => {
            let null_key = vdupq_n_u32(NULL_KEY);
            let weights = byte_weights();
            for byte in 0..8 {
                let (low, high) = lane_masks((bits >> (byte * 8)) as u8, weights);
                for (group, lanes) in [(byte * 2, low), (byte * 2 + 1, high)] {
                    let key = vreinterpretq_u32_s32(load(group));
                    one(group, vbslq_u32(lanes, key, null_key));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{combine, combine_nulls, hash, hash_nulls};
    use crate::int32::{Side, hash_combine, murmurhash32};

    fn keys() -> [i32; 64] {
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        std::array::from_fn(|lane| {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            match lane % 8 {
                0 => i32::MIN,
                1 => i32::MAX,
                2 => 0,
                3 => -1,
                _ => (state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 32) as i32,
            }
        })
    }

    #[test]
    fn lanes_match_the_scalar_functions_on_both_storages() {
        let dense = keys();
        let datums = dense.map(|key| i64::from(key) as u64);
        let non_nulls = 0xF0F0_0FF0_1234_5678_u64;
        let previous: [u32; 64] =
            std::array::from_fn(|lane| (lane as u32).wrapping_mul(0x9e37_79b9));
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
            hash(keys, &mut out);
            assert_eq!(out, expected_hash);
            hash_nulls(keys, non_nulls, &mut out);
            assert_eq!(out, expected_hash_nulls);
            out = previous;
            combine(keys, &mut out);
            assert_eq!(out, expected_combine);
            out = previous;
            combine_nulls(keys, non_nulls, &mut out);
            assert_eq!(out, expected_combine_nulls);
        }
    }
}
