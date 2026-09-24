//! Whole-word int8 key hashes: every lane folded to 32 bits as
//! PostgreSQL's `hashint8` folds it, then hashed and combined as the int4
//! lanes of [`super::hash`].
//!
//! Dense int8 storage and Datum storage are the same eight bytes per row,
//! so one loader serves both: four keys are two 128-bit loads, `uzp1` and
//! `uzp2` gather their low and high halves, and the fold is
//! `low ^ high ^ (high >> 31)` with an arithmetic shift, the high half
//! inverted for negative keys.

use core::arch::aarch64::{
    int32x4_t, uint32x4_t, vaddq_u32, vdupq_n_u32, veorq_u32, vld1q_u64, vreinterpretq_s32_u32,
    vreinterpretq_u32_s32, vreinterpretq_u32_u64, vshlq_n_u32, vshrq_n_s32, vshrq_n_u32,
    vuzp1q_u32, vuzp2q_u32,
};

use super::hash::groups;
use crate::int64::Side;

/// The hash of every lane's key into `out`.
#[inline]
pub fn hash64(keys: Side<'_>, out: &mut [u32; 64]) {
    const { assert!(cfg!(target_feature = "neon")) }
    // SAFETY: NEON is enabled for this compilation (asserted above).
    unsafe { hash_lanes(keys, None, out) }
}

/// As [`hash64`], with NULL lanes hashing the group key.
#[inline]
pub fn hash_nulls64(keys: Side<'_>, non_nulls: u64, out: &mut [u32; 64]) {
    const { assert!(cfg!(target_feature = "neon")) }
    // SAFETY: as in hash64.
    unsafe { hash_lanes(keys, Some(non_nulls), out) }
}

/// Every lane's key hash folded into its previous hash in `out`.
#[inline]
pub fn combine64(keys: Side<'_>, out: &mut [u32; 64]) {
    const { assert!(cfg!(target_feature = "neon")) }
    // SAFETY: as in hash64.
    unsafe { combine_lanes(keys, None, out) }
}

/// As [`combine64`], with NULL lanes folding in the group key's hash.
#[inline]
pub fn combine_nulls64(keys: Side<'_>, non_nulls: u64, out: &mut [u32; 64]) {
    const { assert!(cfg!(target_feature = "neon")) }
    // SAFETY: as in hash64.
    unsafe { combine_lanes(keys, Some(non_nulls), out) }
}

#[target_feature(enable = "neon")]
fn hash_lanes(keys: Side<'_>, non_nulls: Option<u64>, out: &mut [u32; 64]) {
    groups(folded(words(keys)), non_nulls, out, |key_hash, _| key_hash)
}

#[target_feature(enable = "neon")]
fn combine_lanes(keys: Side<'_>, non_nulls: Option<u64>, out: &mut [u32; 64]) {
    let golden = vdupq_n_u32(0x9e37_79b9);
    groups(folded(words(keys)), non_nulls, out, |key_hash, previous| {
        // a ^ (b + 0x9e3779b9 + (a << 6) + (a >> 2)), as hash_combine.
        let mut t = vaddq_u32(key_hash, golden);
        t = vaddq_u32(t, vshlq_n_u32::<6>(previous));
        t = vaddq_u32(t, vshrq_n_u32::<2>(previous));
        veorq_u32(previous, t)
    })
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

/// A loader of the folded keys of each group of four lanes.
#[inline]
#[target_feature(enable = "neon")]
fn folded(words: &[u64; 64]) -> impl Fn(usize) -> int32x4_t + '_ {
    move |group| {
        // SAFETY: `group` is below 16, so the four words read end within
        // the array.
        let (first, second) = unsafe {
            let start = words.as_ptr().add(group * 4);
            (vld1q_u64(start), vld1q_u64(start.add(2)))
        };
        let (first, second) = (vreinterpretq_u32_u64(first), vreinterpretq_u32_u64(second));
        let low: uint32x4_t = vuzp1q_u32(first, second);
        let high: uint32x4_t = vuzp2q_u32(first, second);
        let sign = vreinterpretq_u32_s32(vshrq_n_s32::<31>(vreinterpretq_s32_u32(high)));
        vreinterpretq_s32_u32(veorq_u32(low, veorq_u32(high, sign)))
    }
}

#[cfg(test)]
mod tests {
    use super::{combine_nulls64, combine64, hash_nulls64, hash64};
    use crate::int64::{Side, fold, hash_combine, murmurhash32};

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
        for side in [Side::Dense(&keys), Side::Datum(&words)] {
            let mut out = [0; 64];
            hash64(side, &mut out);
            assert_eq!(out, keys.map(|key| murmurhash32(fold(key))));
            let mut grouped = [0; 64];
            hash_nulls64(side, non_nulls, &mut grouped);
            let mut previous = [0x1234_5678; 64];
            combine64(side, &mut previous);
            let mut previous_nulls = [0x1234_5678; 64];
            combine_nulls64(side, non_nulls, &mut previous_nulls);
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
