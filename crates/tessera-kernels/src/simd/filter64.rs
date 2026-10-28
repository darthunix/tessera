//! Whole-word comparisons of int8 values with a scalar, and of two int8
//! columns, written once over the primitives of [`Vectors`].
//!
//! A Datum holds an int8 whole, so the Datum block loads like the dense
//! one, without deinterleaving.

use super::{Neon, Vectors};
use crate::int64::Side;
use crate::ops::CompareOp;

/// Rows of a dense block whose value satisfies `value op scalar`, as bits in
/// row order. NULL rows compare like any other and are masked by the caller.
#[inline]
pub fn filter_dense64(neon: Neon, values: &[i64; 64], scalar: i64, op: CompareOp) -> u64 {
    // SAFETY: the token proves that this processor runs NEON.
    unsafe { dense(neon, values, scalar, op) }
}

/// Rows of a Datum block whose int8 value satisfies `value op scalar` and
/// whose flag is not NULL, as bits in row order.
#[inline]
pub fn filter_datum64(
    neon: Neon,
    values: &[u64; 64],
    isnull: &[bool; 64],
    scalar: i64,
    op: CompareOp,
) -> u64 {
    // SAFETY: as in filter_dense64.
    unsafe { datum(neon, values, isnull, scalar, op) }
}

/// The lanes where `left op right` over two whole-word sides of int8
/// columns: dense or Datum storage, the two in any combination; a Datum
/// is the whole word, so both load alike.
#[inline]
pub fn compare_sides64(neon: Neon, left: Side<'_>, right: Side<'_>, op: CompareOp) -> u64 {
    // SAFETY: the token proves that this processor runs NEON.
    unsafe { sides(neon, side_words(left), side_words(right), op) }
}

/// The 64 eight-byte words of a side, dense or Datum alike.
#[inline(always)]
fn side_words(side: Side<'_>) -> &[i64; 64] {
    match side {
        Side::Dense(values) => values,
        // SAFETY: `[u64; 64]` and `[i64; 64]` have the same size and
        // alignment, and every bit pattern is valid for both.
        Side::Datum(values) => unsafe { &*values.as_ptr().cast::<[i64; 64]>() },
        Side::Scalar(_) => unreachable!("a scalar side"),
    }
}

// The functions compiled for the vector set of the target, into which the
// kernels below are inlined, as in `filter.rs`. They take the token of
// NEON while only NEON has this family open: elsewhere it has no values,
// so they cannot be called.

#[cfg_attr(
    all(target_arch = "aarch64", not(miri)),
    target_feature(enable = "neon")
)]
#[cfg_attr(
    all(target_arch = "x86_64", not(miri)),
    target_feature(enable = "avx2")
)]
unsafe fn dense(neon: Neon, values: &[i64; 64], scalar: i64, op: CompareOp) -> u64 {
    dense_word(neon, values, scalar, op)
}

#[cfg_attr(
    all(target_arch = "aarch64", not(miri)),
    target_feature(enable = "neon")
)]
#[cfg_attr(
    all(target_arch = "x86_64", not(miri)),
    target_feature(enable = "avx2")
)]
unsafe fn datum(
    neon: Neon,
    values: &[u64; 64],
    isnull: &[bool; 64],
    scalar: i64,
    op: CompareOp,
) -> u64 {
    datum_word(neon, values, isnull, scalar, op)
}

#[cfg_attr(
    all(target_arch = "aarch64", not(miri)),
    target_feature(enable = "neon")
)]
#[cfg_attr(
    all(target_arch = "x86_64", not(miri)),
    target_feature(enable = "avx2")
)]
unsafe fn sides(neon: Neon, left: &[i64; 64], right: &[i64; 64], op: CompareOp) -> u64 {
    sides_word(neon, left, right, op)
}

#[inline(always)]
fn dense_word<V: Vectors>(v: V, values: &[i64; 64], scalar: i64, op: CompareOp) -> u64 {
    // SAFETY: `pack64` asks for the vectors of a word only.
    pack(v, op, scalar, |index| unsafe { v.load64(values, index) })
}

#[inline(always)]
fn datum_word<V: Vectors>(
    v: V,
    values: &[u64; 64],
    isnull: &[bool; 64],
    scalar: i64,
    op: CompareOp,
) -> u64 {
    // SAFETY: as in dense_word.
    pack(v, op, scalar, |index| unsafe {
        v.load_datums64(values, index)
    }) & v.non_null_bits(isnull)
}

#[inline(always)]
fn sides_word<V: Vectors>(v: V, left: &[i64; 64], right: &[i64; 64], op: CompareOp) -> u64 {
    // SAFETY, for both loaders: `pack64` asks for the vectors of a word
    // only, which lie within the 64 words of each side.
    let l = |index: usize| unsafe { v.load64(left, index) };
    let r = |index: usize| unsafe { v.load64(right, index) };
    match op {
        CompareOp::Eq => v.pack64(false, |i| v.eq64(l(i), r(i))),
        CompareOp::Ne => v.pack64(true, |i| v.eq64(l(i), r(i))),
        CompareOp::Lt => v.pack64(false, |i| v.lt64(l(i), r(i))),
        CompareOp::Le => v.pack64(false, |i| v.le64(l(i), r(i))),
        CompareOp::Gt => v.pack64(false, |i| v.gt64(l(i), r(i))),
        CompareOp::Ge => v.pack64(false, |i| v.ge64(l(i), r(i))),
    }
}

/// Compare the vectors of a word with a scalar and pack the results into
/// 64 bits, with the comparison chosen once per word; `Ne` packs the
/// inverse of `Eq`.
#[inline(always)]
fn pack<V: Vectors>(v: V, op: CompareOp, scalar: i64, load: impl Fn(usize) -> V::I64) -> u64 {
    let scalar = v.splat64(scalar);
    match op {
        CompareOp::Eq => v.pack64(false, |i| v.eq64(load(i), scalar)),
        CompareOp::Ne => v.pack64(true, |i| v.eq64(load(i), scalar)),
        CompareOp::Lt => v.pack64(false, |i| v.lt64(load(i), scalar)),
        CompareOp::Le => v.pack64(false, |i| v.le64(load(i), scalar)),
        CompareOp::Gt => v.pack64(false, |i| v.gt64(load(i), scalar)),
        CompareOp::Ge => v.pack64(false, |i| v.ge64(load(i), scalar)),
    }
}
