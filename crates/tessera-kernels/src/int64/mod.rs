//! Signed int64 kernels without PostgreSQL type dispatch: comparisons
//! ([`filter`]), arithmetic ([`arith_scalar`], [`arith_scalar_left`],
//! [`arith_columns`]), aggregates ([`count`], [`min`], [`max`]; no sum,
//! which PostgreSQL computes in numeric) and key hashes ([`hash`],
//! [`hash_next`]), which agree with the int4 hashes on the int4 range.
//!
//! The family shares with [`crate::int32`] the vocabulary of [`crate::ops`]
//! and the drivers of `crate::int`, generic over the lane type, which each
//! kernel moves to as a commit of its own (plan 4.25): the comparisons of a
//! column with a scalar and of two columns and the extremes so far; the
//! others still mirror int32 kernel by kernel. What stays the family's own
//! is the vector code of a whole word (`simd`), whose lanes and gaps differ. A physical int64 representation does not select
//! PostgreSQL semantics: the caller chooses kernels by logical type and
//! operation, and a Datum holds an int8 as its whole word.

mod aggregate;
mod arith;
mod compare;
mod divisor;
mod filter;
mod hash;

pub use aggregate::{max, min};
pub(crate) use arith::Side;
pub use arith::{arith_columns, arith_scalar, arith_scalar_left};
pub use compare::compare_columns;
pub(crate) use divisor::Divisor;
pub use filter::filter;
pub use hash::{NullKeys, fold, hash, hash_combine, hash_next, murmurhash32};

pub use crate::ops::{ArithOp, ArithmeticError, CompareOp};

pub(crate) use crate::BULK_MIN_ROWS;

use tessera_core::WordBlock;

impl crate::int::IntLane for i64 {
    const MIN: i64 = i64::MIN;
    const MAX: i64 = i64::MAX;

    #[inline(always)]
    fn from_datum(word: u64) -> i64 {
        word as i64
    }

    #[cfg(all(target_arch = "aarch64", not(miri)))]
    fn side(block: WordBlock<'_, Self>) -> (Side<'_>, u64) {
        match block {
            WordBlock::Dense { values, non_nulls } => (Side::Dense(values), non_nulls),
            WordBlock::Datum { values, isnull } => {
                (Side::Datum(values), crate::simd::non_null_bits(isnull))
            }
        }
    }

    #[inline(always)]
    fn compare_sides(left: Side<'_>, right: Side<'_>, op: CompareOp) -> u64 {
        #[cfg(all(target_arch = "aarch64", not(miri)))]
        return crate::simd::compare_sides64(left, right, op);
        #[cfg(not(all(target_arch = "aarch64", not(miri))))]
        {
            let _ = (left, right, op);
            unreachable!("no whole-word kernels on this target")
        }
    }

    #[inline(always)]
    fn filter_block(block: WordBlock<'_, Self>, scalar: Self, op: CompareOp) -> u64 {
        #[cfg(all(target_arch = "aarch64", not(miri)))]
        return match block {
            WordBlock::Dense { values, non_nulls } => {
                crate::simd::filter_dense64(values, scalar, op) & non_nulls
            }
            WordBlock::Datum { values, isnull } => {
                crate::simd::filter_datum64(values, isnull, scalar, op)
            }
        };
        #[cfg(not(all(target_arch = "aarch64", not(miri))))]
        {
            let _ = (block, scalar, op);
            unreachable!("no whole-word kernels on this target")
        }
    }

    #[inline]
    fn min_block(block: WordBlock<'_, Self>, selected: u64) -> (usize, i64) {
        #[cfg(all(target_arch = "aarch64", not(miri)))]
        {
            let mask = crate::int::present(&block, selected);
            let least = match block {
                WordBlock::Dense { values, .. } => crate::simd::min_dense64(values, mask),
                WordBlock::Datum { values, .. } => crate::simd::min_datum64(values, mask),
            };
            (mask.count_ones() as usize, least)
        }
        #[cfg(not(all(target_arch = "aarch64", not(miri))))]
        {
            let _ = (block, selected);
            unreachable!("no whole-word kernels on this target")
        }
    }

    #[inline]
    fn max_block(block: WordBlock<'_, Self>, selected: u64) -> (usize, i64) {
        #[cfg(all(target_arch = "aarch64", not(miri)))]
        {
            let mask = crate::int::present(&block, selected);
            let greatest = match block {
                WordBlock::Dense { values, .. } => crate::simd::max_dense64(values, mask),
                WordBlock::Datum { values, .. } => crate::simd::max_datum64(values, mask),
            };
            (mask.count_ones() as usize, greatest)
        }
        #[cfg(not(all(target_arch = "aarch64", not(miri))))]
        {
            let _ = (block, selected);
            unreachable!("no whole-word kernels on this target")
        }
    }
}
