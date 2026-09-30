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

mod compare;
mod filter;

pub(crate) use compare::compare_columns;
pub(crate) use filter::filter;

use tessera_core::WordBlock;

use crate::ops::CompareOp;

/// A whole-word operand: the storage of a full prepared word, or a constant.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Side<'a, T> {
    Dense(&'a [T; 64]),
    Datum(&'a [u64; 64]),
    Scalar(T),
}

impl<T: IntLane> Side<'_, T> {
    /// One lane of the width, a Datum's as the width reads it.
    #[inline(always)]
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
    /// The value a Datum holds: its low 32 bits for int4, as
    /// `DatumGetInt32`, its whole word for int8, as `DatumGetInt64`.
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
    fn filter_block(block: WordBlock<'_, Self>, scalar: Self, op: CompareOp) -> u64;
}
