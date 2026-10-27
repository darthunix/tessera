//! Whole-word comparisons of int4 values with a scalar, and of two
//! columns, written once over the primitives of [`Vectors`].

use super::{Neon, Vectors};
use crate::int32::{CompareOp, Side};

/// Rows of a dense block whose value satisfies `value op scalar`, as bits in
/// row order. NULL rows compare like any other and are masked by the caller.
#[inline]
pub fn filter_dense(neon: Neon, values: &[i32; 64], scalar: i32, op: CompareOp) -> u64 {
    // SAFETY: the token proves that this processor runs NEON.
    unsafe { dense(neon, values, scalar, op) }
}

/// Rows of a Datum block whose int4 value satisfies `value op scalar` and
/// whose flag is not NULL, as bits in row order.
#[inline]
pub fn filter_datum(
    neon: Neon,
    values: &[u64; 64],
    isnull: &[bool; 64],
    scalar: i32,
    op: CompareOp,
) -> u64 {
    // SAFETY: as in filter_dense.
    unsafe { datum(neon, values, isnull, scalar, op) }
}

/// The lanes where `left op right` over two whole-word sides of columns:
/// dense or Datum storage, the two in any combination.
#[inline]
pub fn compare_sides(neon: Neon, left: Side<'_>, right: Side<'_>, op: CompareOp) -> u64 {
    // SAFETY: the token proves that this processor runs NEON.
    unsafe { sides(neon, left, right, op) }
}

// The functions compiled for NEON, into which the kernels below are
// inlined.

#[target_feature(enable = "neon")]
fn dense(neon: Neon, values: &[i32; 64], scalar: i32, op: CompareOp) -> u64 {
    dense_word(neon, values, scalar, op)
}

#[target_feature(enable = "neon")]
fn datum(neon: Neon, values: &[u64; 64], isnull: &[bool; 64], scalar: i32, op: CompareOp) -> u64 {
    datum_word(neon, values, isnull, scalar, op)
}

#[target_feature(enable = "neon")]
fn sides(neon: Neon, left: Side<'_>, right: Side<'_>, op: CompareOp) -> u64 {
    sides_word(neon, left, right, op)
}

#[inline(always)]
fn dense_word<V: Vectors>(v: V, values: &[i32; 64], scalar: i32, op: CompareOp) -> u64 {
    // SAFETY: `pack` asks for the groups of a word only.
    pack(v, op, scalar, |group| unsafe { v.load(values, group) })
}

#[inline(always)]
fn datum_word<V: Vectors>(
    v: V,
    values: &[u64; 64],
    isnull: &[bool; 64],
    scalar: i32,
    op: CompareOp,
) -> u64 {
    // SAFETY: as in dense_word.
    pack(v, op, scalar, |group| unsafe {
        v.load_datums(values, group)
    }) & v.non_null_bits(isnull)
}

#[inline(always)]
fn sides_word<V: Vectors>(v: V, left: Side<'_>, right: Side<'_>, op: CompareOp) -> u64 {
    // One instance per storage pair, so that the loaders fold into the loop.
    match (left, right) {
        (Side::Dense(_), Side::Dense(_))
        | (Side::Dense(_), Side::Datum(_))
        | (Side::Datum(_), Side::Dense(_))
        | (Side::Datum(_), Side::Datum(_)) => {
            pack_pairs(v, op, side_loader(v, left), side_loader(v, right))
        }
        _ => unreachable!("a scalar side"),
    }
}

/// A loader of the lanes of a group of a column side.
#[inline(always)]
fn side_loader<V: Vectors>(v: V, side: Side<'_>) -> impl Fn(usize) -> V::I32 + '_ {
    move |group| match side {
        // SAFETY: `pack` asks for the groups of a word only, which lie
        // within the 64 rows of the side.
        Side::Dense(values) => unsafe { v.load(values, group) },
        // SAFETY: as above, for the loader of Datum words.
        Side::Datum(values) => unsafe { v.load_datums(values, group) },
        Side::Scalar(_) => unreachable!("a scalar side"),
    }
}

#[inline(always)]
fn pack_pairs<V: Vectors>(
    v: V,
    op: CompareOp,
    left: impl Fn(usize) -> V::I32,
    right: impl Fn(usize) -> V::I32,
) -> u64 {
    match op {
        CompareOp::Eq => v.pack(|g| v.eq(left(g), right(g))),
        CompareOp::Ne => v.pack(|g| v.not(v.eq(left(g), right(g)))),
        CompareOp::Lt => v.pack(|g| v.lt(left(g), right(g))),
        CompareOp::Le => v.pack(|g| v.le(left(g), right(g))),
        CompareOp::Gt => v.pack(|g| v.gt(left(g), right(g))),
        CompareOp::Ge => v.pack(|g| v.ge(left(g), right(g))),
    }
}

/// Compare the groups of a word with a scalar and pack the results into
/// 64 bits, with the comparison chosen once per word.
#[inline(always)]
fn pack<V: Vectors>(v: V, op: CompareOp, scalar: i32, load: impl Fn(usize) -> V::I32) -> u64 {
    let scalar = v.splat(scalar);
    match op {
        CompareOp::Eq => pack_with(v, load, |x| v.eq(x, scalar)),
        CompareOp::Ne => pack_with(v, load, |x| v.not(v.eq(x, scalar))),
        CompareOp::Lt => pack_with(v, load, |x| v.lt(x, scalar)),
        CompareOp::Le => pack_with(v, load, |x| v.le(x, scalar)),
        CompareOp::Gt => pack_with(v, load, |x| v.gt(x, scalar)),
        CompareOp::Ge => pack_with(v, load, |x| v.ge(x, scalar)),
    }
}

/// The bits of the 64 rows where `compare` holds of the loaded lanes.
#[inline(always)]
fn pack_with<V: Vectors>(
    v: V,
    load: impl Fn(usize) -> V::I32,
    compare: impl Fn(V::I32) -> V::M32,
) -> u64 {
    v.pack(|group| compare(load(group)))
}
