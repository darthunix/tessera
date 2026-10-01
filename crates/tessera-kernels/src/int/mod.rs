//! The drivers the int4 and int8 families share, generic over the lane
//! type ([`IntLane`]): the choice between whole words and rows, the row
//! loops and the word loops around the vector code. What differs between
//! the widths is the lane type's own: how a Datum holds it and the vector
//! code of a whole word, whose lanes, intrinsics and gaps (NEON has no
//! 64-bit multiplication) are the width's. Each driver is monomorphized
//! per width and keeps its `#[inline(never)]` boundaries, so the code of a
//! width is the code its own copy compiled to, which the disassembly of
//! both widths was compared against when a driver moved here.
//!
//! [`crate::int32`] and [`crate::int64`] keep the public entry points, each
//! a call of the driver for its width.

mod aggregate;
mod arith;
mod compare;
mod filter;
mod hash;

#[cfg(all(target_arch = "aarch64", not(miri)))]
pub(crate) use aggregate::present;
pub(crate) use aggregate::{aggregate, max, min};
pub(crate) use arith::{Evaluate, arith_columns, arith_scalar, arith_scalar_left};
pub(crate) use compare::compare_columns;
pub(crate) use filter::filter;
pub(crate) use hash::{hash, hash_next};

use tessera_core::WordBlock;

use std::mem::MaybeUninit;

use anyhow::Result;

use crate::ops::{ArithOp, ArithmeticError, CompareOp};

/// A whole-word operand: the storage of a full prepared word, or a constant.
/// Only the vector code builds one, so elsewhere it is dead.
#[derive(Clone, Copy, Debug)]
#[cfg_attr(not(all(target_arch = "aarch64", not(miri))), allow(dead_code))]
pub(crate) enum Side<'a, T> {
    Dense(&'a [T; 64]),
    Datum(&'a [u64; 64]),
    Scalar(T),
}

impl<T: IntLane> Side<'_, T> {
    /// One lane of the width, a Datum's as the width reads it.
    #[inline(always)]
    #[cfg_attr(not(all(target_arch = "aarch64", not(miri))), allow(dead_code))]
    pub(crate) fn lane(self, lane: usize) -> T {
        match self {
            Self::Dense(values) => values[lane],
            Self::Datum(values) => T::from_datum(values[lane]),
            Self::Scalar(value) => value,
        }
    }
}

/// A lane type of the integer kernels, int4's `i32` or int8's `i64`: its
/// Datum and its vector code of a whole word. The vector code runs only
/// where the drivers found whole words, on AArch64; elsewhere it is never
/// called.
pub(crate) trait IntLane: Copy + Ord + core::fmt::Debug + 'static {
    /// A scalar divisor prepared for division by multiplication.
    type Divisor;

    /// The least and the greatest value, the identities of `max` and `min`.
    const MIN: Self;
    const MAX: Self;
    /// The placeholder pair of a NULL row in the arithmetic, `ZERO op ONE`,
    /// which fails in no operation.
    const ZERO: Self;
    const ONE: Self;
    /// The width's operations on two non-NULL values, as PostgreSQL's
    /// operators of the width define them: an overflow fails with the
    /// width's out-of-range error, a zero divisor with division by zero,
    /// and `x % -1` is 0. Each is the width's own function, not a generic
    /// one over checked operations, whose `Option` the row loops kept.
    fn add(a: Self, b: Self) -> Result<Self, ArithmeticError>;
    fn sub(a: Self, b: Self) -> Result<Self, ArithmeticError>;
    fn mul(a: Self, b: Self) -> Result<Self, ArithmeticError>;
    fn div(a: Self, b: Self) -> Result<Self, ArithmeticError>;
    fn rem(a: Self, b: Self) -> Result<Self, ArithmeticError>;

    /// A scalar divisor of magnitude at least two, prepared once per call;
    /// `None` for 0 and ±1.
    fn divisor(d: Self) -> Option<Self::Divisor>;

    /// `lhs op rhs` over the present rows of a whole word into `out`, with
    /// the prepared divisor of a division by a scalar: each width's own mix
    /// of vector code and lanes one by one. An overflow fails with
    /// `OUT_OF_RANGE`.
    fn arith_block<E: Evaluate<Self>>(
        op: ArithOp,
        lhs: Side<'_, Self>,
        rhs: Side<'_, Self>,
        present: u64,
        divisor: &Option<Self::Divisor>,
        out: &mut [MaybeUninit<Self>; 64],
        evaluate: &E,
    ) -> Result<()>;

    /// The value a Datum holds: its low 32 bits for int4, as
    /// `DatumGetInt32`, its whole word for int8, as `DatumGetInt64`.
    #[cfg_attr(not(all(target_arch = "aarch64", not(miri))), allow(dead_code))]
    fn from_datum(word: u64) -> Self;

    /// A whole word of a column as an operand, with its non-NULL rows.
    /// Implemented for each width, not generically, so that it is one
    /// function the drivers of every column type call, as it was when each
    /// family had its own.
    #[cfg(all(target_arch = "aarch64", not(miri)))]
    fn side(block: WordBlock<'_, Self>) -> (Side<'_, Self>, u64);

    /// The rows of a whole word where `left op right`, NULL rows
    /// included; the caller masks them.
    fn compare_sides(left: Side<'_, Self>, right: Side<'_, Self>, op: CompareOp) -> u64;

    /// The non-NULL rows of a whole word where `value op scalar`.
    #[cfg_attr(not(all(target_arch = "aarch64", not(miri))), allow(dead_code))]
    fn filter_block(block: WordBlock<'_, Self>, scalar: Self, op: CompareOp) -> u64;

    /// A key's 32 bits for its hash: int4's value, int8's folded as
    /// `hashint8` folds it.
    fn key(self) -> u32;

    /// The hashes of a whole word's keys into `out`; with `_nulls`, a NULL
    /// row's hash is the group key's; with `combine`, each is folded into
    /// the hash already in `out`.
    fn hash_block(keys: Side<'_, Self>, out: &mut [u32; 64]);
    fn hash_nulls_block(keys: Side<'_, Self>, non_null: u64, out: &mut [u32; 64]);
    fn combine_block(keys: Side<'_, Self>, out: &mut [u32; 64]);
    fn combine_nulls_block(keys: Side<'_, Self>, non_null: u64, out: &mut [u32; 64]);

    /// The count of a whole word's selected non-NULL rows and the least of
    /// them, `MAX` without any.
    fn min_block(block: WordBlock<'_, Self>, selected: u64) -> (usize, Self);

    /// The count of a whole word's selected non-NULL rows and the greatest
    /// of them, `MIN` without any.
    fn max_block(block: WordBlock<'_, Self>, selected: u64) -> (usize, Self);
}
