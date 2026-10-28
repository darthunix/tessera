//! Whole-word sums and extremes of int4 values under a row mask, and the
//! count of a Datum block's selected non-NULL rows, written once over the
//! primitives of [`Vectors`].
//!
//! `mask` names the rows to aggregate: selected and non-NULL, computed by
//! the caller (for Datum blocks from [`Vectors::non_null_bits`]). A NULL
//! row's placeholder value is loaded but masked away. A full mask skips
//! the masking; the extremes are meaningful only for a nonzero mask.

// Under Miri no set runs, so the tokens have no values, and the code that
// the functions compiled for a set run on theirs cannot be reached.
#![cfg_attr(miri, allow(unreachable_code, unused_variables))]

use super::{Simd, Vectors};

// The public entry points are inlined into the generic loops of other
// crates, so that a block reaches them in registers; the feature-carrying
// kernels behind them stay out of line.

/// Selected non-NULL rows of a Datum block.
#[inline]
pub fn count_datum(simd: Simd, isnull: &[bool; 64], selected: u64) -> usize {
    // SAFETY: the token proves that this processor runs the vector set of
    // the target, which `count_datum_lanes` is compiled for.
    unsafe { count_datum_lanes(simd, isnull, selected) }
}

/// Sum of the masked rows of a dense block.
#[inline]
pub fn sum_dense(simd: Simd, values: &[i32; 64], mask: u64) -> i64 {
    // SAFETY: as in count_datum, for `sum_dense_lanes`.
    unsafe { sum_dense_lanes(simd, values, mask) }
}

/// Sum of the masked rows of a Datum block, as int4 values.
#[inline]
pub fn sum_datum(simd: Simd, values: &[u64; 64], mask: u64) -> i64 {
    // SAFETY: as in count_datum, for `sum_datum_lanes`.
    unsafe { sum_datum_lanes(simd, values, mask) }
}

/// Least masked row of a dense block; `i32::MAX` for an empty mask.
#[inline]
pub fn min_dense(simd: Simd, values: &[i32; 64], mask: u64) -> i32 {
    // SAFETY: as in count_datum, for `min_dense_lanes`.
    unsafe { min_dense_lanes(simd, values, mask) }
}

/// Greatest masked row of a dense block; `i32::MIN` for an empty mask.
#[inline]
pub fn max_dense(simd: Simd, values: &[i32; 64], mask: u64) -> i32 {
    // SAFETY: as in count_datum, for `max_dense_lanes`.
    unsafe { max_dense_lanes(simd, values, mask) }
}

/// Least masked row of a Datum block; `i32::MAX` for an empty mask.
#[inline]
pub fn min_datum(simd: Simd, values: &[u64; 64], mask: u64) -> i32 {
    // SAFETY: as in count_datum, for `min_datum_lanes`.
    unsafe { min_datum_lanes(simd, values, mask) }
}

/// Greatest masked row of a Datum block; `i32::MIN` for an empty mask.
#[inline]
pub fn max_datum(simd: Simd, values: &[u64; 64], mask: u64) -> i32 {
    // SAFETY: as in count_datum, for `max_datum_lanes`.
    unsafe { max_datum_lanes(simd, values, mask) }
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
unsafe fn count_datum_lanes(simd: Simd, isnull: &[bool; 64], selected: u64) -> usize {
    simd.count_non_null(isnull, selected)
}

#[cfg_attr(
    all(target_arch = "aarch64", not(miri)),
    target_feature(enable = "neon")
)]
#[cfg_attr(
    all(target_arch = "x86_64", not(miri)),
    target_feature(enable = "avx2")
)]
unsafe fn sum_dense_lanes(simd: Simd, values: &[i32; 64], mask: u64) -> i64 {
    // SAFETY: `sum_word` asks for the groups of a word only.
    let load = |group: usize| unsafe { simd.load(values, group) };
    sum_word(simd, load, mask)
}

#[cfg_attr(
    all(target_arch = "aarch64", not(miri)),
    target_feature(enable = "neon")
)]
#[cfg_attr(
    all(target_arch = "x86_64", not(miri)),
    target_feature(enable = "avx2")
)]
unsafe fn sum_datum_lanes(simd: Simd, values: &[u64; 64], mask: u64) -> i64 {
    // SAFETY: as in sum_dense_lanes.
    let load = |group: usize| unsafe { simd.load_datums(values, group) };
    sum_word(simd, load, mask)
}

#[cfg_attr(
    all(target_arch = "aarch64", not(miri)),
    target_feature(enable = "neon")
)]
#[cfg_attr(
    all(target_arch = "x86_64", not(miri)),
    target_feature(enable = "avx2")
)]
unsafe fn min_dense_lanes(simd: Simd, values: &[i32; 64], mask: u64) -> i32 {
    // SAFETY: `extreme_word` asks for the groups of a word only.
    let load = |group: usize| unsafe { simd.load(values, group) };
    simd.min_across(extreme_word(simd, load, mask, i32::MAX, |a, b| {
        simd.min(a, b)
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
unsafe fn max_dense_lanes(simd: Simd, values: &[i32; 64], mask: u64) -> i32 {
    // SAFETY: as in min_dense_lanes.
    let load = |group: usize| unsafe { simd.load(values, group) };
    simd.max_across(extreme_word(simd, load, mask, i32::MIN, |a, b| {
        simd.max(a, b)
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
unsafe fn min_datum_lanes(simd: Simd, values: &[u64; 64], mask: u64) -> i32 {
    // SAFETY: as in min_dense_lanes.
    let load = |group: usize| unsafe { simd.load_datums(values, group) };
    simd.min_across(extreme_word(simd, load, mask, i32::MAX, |a, b| {
        simd.min(a, b)
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
unsafe fn max_datum_lanes(simd: Simd, values: &[u64; 64], mask: u64) -> i32 {
    // SAFETY: as in min_dense_lanes.
    let load = |group: usize| unsafe { simd.load_datums(values, group) };
    simd.max_across(extreme_word(simd, load, mask, i32::MIN, |a, b| {
        simd.max(a, b)
    }))
}

/// Widening adds into four accumulators: a chain of a quarter of the
/// groups per word instead of all of them, which on an M5 Pro halves the
/// cycles of a full word; masked lanes are zeroed first.
#[inline]
#[cfg_attr(
    all(target_arch = "aarch64", not(miri)),
    target_feature(enable = "neon")
)]
#[cfg_attr(
    all(target_arch = "x86_64", not(miri)),
    target_feature(enable = "avx2")
)]
fn sum_word<V: Vectors>(v: V, load: impl Fn(usize) -> V::I32, mask: u64) -> i64 {
    let zero = v.splat64(0);
    let (mut a, mut b, mut c, mut d) = (zero, zero, zero, zero);
    if mask == u64::MAX {
        for step in 0..64 / (4 * V::LANES) {
            a = v.widen_add(a, load(step * 4));
            b = v.widen_add(b, load(step * 4 + 1));
            c = v.widen_add(c, load(step * 4 + 2));
            d = v.widen_add(d, load(step * 4 + 3));
        }
    } else {
        for step in 0..64 / (4 * V::LANES) {
            let [m0, m1, m2, m3] = v.lane_masks4(mask, step);
            a = v.widen_add(a, v.keep(load(step * 4), m0));
            b = v.widen_add(b, v.keep(load(step * 4 + 1), m1));
            c = v.widen_add(c, v.keep(load(step * 4 + 2), m2));
            d = v.widen_add(d, v.keep(load(step * 4 + 3), m3));
        }
    }
    v.sum_across64(v.wrapping_add64(v.wrapping_add64(a, b), v.wrapping_add64(c, d)))
}

/// Lane-wise `combine` over the groups in four accumulators, with masked
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
    load: impl Fn(usize) -> V::I32,
    mask: u64,
    identity: i32,
    combine: impl Fn(V::I32, V::I32) -> V::I32,
) -> V::I32 {
    let identity = v.splat(identity);
    let (mut a, mut b, mut c, mut d) = (identity, identity, identity, identity);
    if mask == u64::MAX {
        for step in 0..64 / (4 * V::LANES) {
            a = combine(a, load(step * 4));
            b = combine(b, load(step * 4 + 1));
            c = combine(c, load(step * 4 + 2));
            d = combine(d, load(step * 4 + 3));
        }
    } else {
        for step in 0..64 / (4 * V::LANES) {
            let [m0, m1, m2, m3] = v.lane_masks4(mask, step);
            a = combine(a, v.select(m0, load(step * 4), identity));
            b = combine(b, v.select(m1, load(step * 4 + 1), identity));
            c = combine(c, v.select(m2, load(step * 4 + 2), identity));
            d = combine(d, v.select(m3, load(step * 4 + 3), identity));
        }
    }
    combine(combine(a, b), combine(c, d))
}
