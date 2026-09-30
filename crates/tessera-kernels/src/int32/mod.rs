//! Signed int32 kernels without PostgreSQL type dispatch: comparisons
//! ([`filter`]), aggregates ([`sum`], [`min`], [`max`]; the count is
//! [`crate::count::count`]'s), arithmetic ([`arith_scalar`],
//! [`arith_scalar_left`], [`arith_columns`]) and key hashes ([`hash`],
//! [`hash_next`]).
//!
//! A physical int32 representation does not select PostgreSQL semantics:
//! the future caller must choose kernels by logical type and operation.

mod aggregate;
mod arith;
mod compare;
pub(crate) mod divisor;
mod filter;
pub(crate) mod hash;

pub use crate::ops::{ArithOp, ArithmeticError, CompareOp};
pub use aggregate::{max, min, sum};
pub(crate) use arith::Side;
pub use arith::{arith_columns, arith_scalar, arith_scalar_left};
pub use compare::compare_columns;
pub(crate) use divisor::Divisor;
pub use filter::filter;
pub use hash::{NullKeys, hash, hash_combine, hash_next, murmurhash32};

pub(crate) use crate::BULK_MIN_ROWS;

use tessera_core::WordBlock;

impl crate::int::IntLane for i32 {
    const MIN: i32 = i32::MIN;
    const MAX: i32 = i32::MAX;

    #[inline(always)]
    fn from_datum(word: u64) -> i32 {
        word as i32
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
        return crate::simd::compare_sides(left, right, op);
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
                crate::simd::filter_dense(values, scalar, op) & non_nulls
            }
            WordBlock::Datum { values, isnull } => {
                crate::simd::filter_datum(values, isnull, scalar, op)
            }
        };
        #[cfg(not(all(target_arch = "aarch64", not(miri))))]
        {
            let _ = (block, scalar, op);
            unreachable!("no whole-word kernels on this target")
        }
    }

    #[inline]
    fn min_block(block: WordBlock<'_, Self>, selected: u64) -> (usize, i32) {
        #[cfg(all(target_arch = "aarch64", not(miri)))]
        {
            let mask = crate::int::present(&block, selected);
            let least = match block {
                WordBlock::Dense { values, .. } => crate::simd::min_dense(values, mask),
                WordBlock::Datum { values, .. } => crate::simd::min_datum(values, mask),
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
    fn max_block(block: WordBlock<'_, Self>, selected: u64) -> (usize, i32) {
        #[cfg(all(target_arch = "aarch64", not(miri)))]
        {
            let mask = crate::int::present(&block, selected);
            let greatest = match block {
                WordBlock::Dense { values, .. } => crate::simd::max_dense(values, mask),
                WordBlock::Datum { values, .. } => crate::simd::max_datum(values, mask),
            };
            (mask.count_ones() as usize, greatest)
        }
        #[cfg(not(all(target_arch = "aarch64", not(miri))))]
        {
            let _ = (block, selected);
            unreachable!("no whole-word kernels on this target")
        }
    }

    #[inline(always)]
    fn key(self) -> u32 {
        self as u32
    }

    #[inline(always)]
    fn hash_block(keys: Side<'_>, out: &mut [u32; 64]) {
        #[cfg(all(target_arch = "aarch64", not(miri)))]
        crate::simd::hash(keys, out);
        #[cfg(not(all(target_arch = "aarch64", not(miri))))]
        {
            let _ = (keys, out);
            unreachable!("no whole-word kernels on this target")
        }
    }

    #[inline(always)]
    fn hash_nulls_block(keys: Side<'_>, non_null: u64, out: &mut [u32; 64]) {
        #[cfg(all(target_arch = "aarch64", not(miri)))]
        crate::simd::hash_nulls(keys, non_null, out);
        #[cfg(not(all(target_arch = "aarch64", not(miri))))]
        {
            let _ = (keys, non_null, out);
            unreachable!("no whole-word kernels on this target")
        }
    }

    #[inline(always)]
    fn combine_block(keys: Side<'_>, out: &mut [u32; 64]) {
        #[cfg(all(target_arch = "aarch64", not(miri)))]
        crate::simd::combine(keys, out);
        #[cfg(not(all(target_arch = "aarch64", not(miri))))]
        {
            let _ = (keys, out);
            unreachable!("no whole-word kernels on this target")
        }
    }

    #[inline(always)]
    fn combine_nulls_block(keys: Side<'_>, non_null: u64, out: &mut [u32; 64]) {
        #[cfg(all(target_arch = "aarch64", not(miri)))]
        crate::simd::combine_nulls(keys, non_null, out);
        #[cfg(not(all(target_arch = "aarch64", not(miri))))]
        {
            let _ = (keys, non_null, out);
            unreachable!("no whole-word kernels on this target")
        }
    }
}
