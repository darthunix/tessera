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
}
