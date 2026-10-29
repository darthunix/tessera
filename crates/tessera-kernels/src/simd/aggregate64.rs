//! Whole-word extremes of int8 values under a row mask, written once over
//! the primitives of [`Vectors`].
//!
//! `mask` names the rows to aggregate: selected and non-NULL, computed by
//! the caller (for Datum blocks from [`Vectors::non_null_bits`]). A NULL
//! row's placeholder value is loaded but masked away. A full mask skips
//! the masking; the extremes are meaningful only for a nonzero mask.
//! Neither set has a minimum or a maximum of int8 lanes, so a lane-wise
//! extreme is a compare and a select; four accumulators keep the chain
//! short, as for int4.

// Under Miri no set runs, so the tokens have no values, and the code that
// the functions compiled for a set run on theirs cannot be reached.
#![cfg_attr(miri, allow(unreachable_code, unused_variables))]

use super::{Neon, Simd, Vectors};

/// Least masked row of a dense block; `i64::MAX` for an empty mask.
#[inline]
pub fn min_dense64(neon: Neon, values: &[i64; 64], mask: u64) -> i64 {
    // SAFETY: the token proves that this processor runs NEON, the vector
    // set of the target where it has values, which `min_dense_lanes` is
    // compiled for.
    unsafe { min_dense_lanes(neon.simd(), values, mask) }
}

/// Greatest masked row of a dense block; `i64::MIN` for an empty mask.
#[inline]
pub fn max_dense64(neon: Neon, values: &[i64; 64], mask: u64) -> i64 {
    // SAFETY: as in min_dense64, for `max_dense_lanes`.
    unsafe { max_dense_lanes(neon.simd(), values, mask) }
}

/// Least masked row of a Datum block; `i64::MAX` for an empty mask.
#[inline]
pub fn min_datum64(neon: Neon, values: &[u64; 64], mask: u64) -> i64 {
    // SAFETY: as in min_dense64, for `min_datum_lanes`.
    unsafe { min_datum_lanes(neon.simd(), values, mask) }
}

/// Greatest masked row of a Datum block; `i64::MIN` for an empty mask.
#[inline]
pub fn max_datum64(neon: Neon, values: &[u64; 64], mask: u64) -> i64 {
    // SAFETY: as in min_dense64, for `max_datum_lanes`.
    unsafe { max_datum_lanes(neon.simd(), values, mask) }
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
unsafe fn min_dense_lanes(simd: Simd, values: &[i64; 64], mask: u64) -> i64 {
    // SAFETY: `extreme_word` asks for the vectors of a word only.
    let load = |index: usize| unsafe { simd.load64(values, index) };
    simd.min_across64(extreme_word(simd, load, mask, i64::MAX, |a, b| {
        least(simd, a, b)
    }))
}

#[cfg_attr(
    all(target_arch = "aarch64", not(miri)),
    target_feature(enable = "neon")
)]
#[cfg_attr(
    all(target_arch = "x86_64", not(miri)),
    target_feature(enable = "avx2")
)]
unsafe fn max_dense_lanes(simd: Simd, values: &[i64; 64], mask: u64) -> i64 {
    // SAFETY: as in min_dense_lanes.
    let load = |index: usize| unsafe { simd.load64(values, index) };
    simd.max_across64(extreme_word(simd, load, mask, i64::MIN, |a, b| {
        greatest(simd, a, b)
    }))
}

#[cfg_attr(
    all(target_arch = "aarch64", not(miri)),
    target_feature(enable = "neon")
)]
#[cfg_attr(
    all(target_arch = "x86_64", not(miri)),
    target_feature(enable = "avx2")
)]
unsafe fn min_datum_lanes(simd: Simd, values: &[u64; 64], mask: u64) -> i64 {
    // SAFETY: as in min_dense_lanes.
    let load = |index: usize| unsafe { simd.load_datums64(values, index) };
    simd.min_across64(extreme_word(simd, load, mask, i64::MAX, |a, b| {
        least(simd, a, b)
    }))
}

#[cfg_attr(
    all(target_arch = "aarch64", not(miri)),
    target_feature(enable = "neon")
)]
#[cfg_attr(
    all(target_arch = "x86_64", not(miri)),
    target_feature(enable = "avx2")
)]
unsafe fn max_datum_lanes(simd: Simd, values: &[u64; 64], mask: u64) -> i64 {
    // SAFETY: as in min_dense_lanes.
    let load = |index: usize| unsafe { simd.load_datums64(values, index) };
    simd.max_across64(extreme_word(simd, load, mask, i64::MIN, |a, b| {
        greatest(simd, a, b)
    }))
}

/// Lane-wise minimum: where `a > b` take `b`.
#[inline]
#[cfg_attr(
    all(target_arch = "aarch64", not(miri)),
    target_feature(enable = "neon")
)]
#[cfg_attr(
    all(target_arch = "x86_64", not(miri)),
    target_feature(enable = "avx2")
)]
fn least<V: Vectors>(v: V, a: V::I64, b: V::I64) -> V::I64 {
    v.select64(v.gt64(a, b), b, a)
}

/// Lane-wise maximum: where `a > b` take `a`.
#[inline]
#[cfg_attr(
    all(target_arch = "aarch64", not(miri)),
    target_feature(enable = "neon")
)]
#[cfg_attr(
    all(target_arch = "x86_64", not(miri)),
    target_feature(enable = "avx2")
)]
fn greatest<V: Vectors>(v: V, a: V::I64, b: V::I64) -> V::I64 {
    v.select64(v.gt64(a, b), a, b)
}

/// Lane-wise `combine` over the vectors in four accumulators, with masked
/// lanes replaced by the operation's identity; the caller reduces the
/// lanes of the result.
#[inline]
#[cfg_attr(
    all(target_arch = "aarch64", not(miri)),
    target_feature(enable = "neon")
)]
#[cfg_attr(
    all(target_arch = "x86_64", not(miri)),
    target_feature(enable = "avx2")
)]
fn extreme_word<V: Vectors>(
    v: V,
    load: impl Fn(usize) -> V::I64,
    mask: u64,
    identity: i64,
    combine: impl Fn(V::I64, V::I64) -> V::I64,
) -> V::I64 {
    let identity = v.splat64(identity);
    let (mut a, mut b, mut c, mut d) = (identity, identity, identity, identity);
    if mask == u64::MAX {
        for step in 0..64 / (4 * V::LANES64) {
            a = combine(a, load(step * 4));
            b = combine(b, load(step * 4 + 1));
            c = combine(c, load(step * 4 + 2));
            d = combine(d, load(step * 4 + 3));
        }
    } else {
        for step in 0..64 / (4 * V::LANES64) {
            let [m0, m1, m2, m3] = v.lane_masks4_64(mask, step);
            a = combine(a, v.select64(m0, load(step * 4), identity));
            b = combine(b, v.select64(m1, load(step * 4 + 1), identity));
            c = combine(c, v.select64(m2, load(step * 4 + 2), identity));
            d = combine(d, v.select64(m3, load(step * 4 + 3), identity));
        }
    }
    combine(combine(a, b), combine(c, d))
}
