//! The int8 lane of the drivers of `crate::int`: how a Datum holds an
//! int8, its arithmetic's checks and error, and its vector code of a
//! whole word.

use std::mem::MaybeUninit;

use anyhow::Result;
use tessera_core::WordBlock;

use super::fold;
use super::{CompareOp, Divisor, Side};
use crate::int::{Evaluate, IntLane};
use crate::ops::{ArithOp, ArithmeticError};

impl IntLane for i64 {
    type Divisor = Divisor;

    const ZERO: i64 = 0;
    const ONE: i64 = 1;
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

    #[inline(always)]
    fn key(self) -> u32 {
        fold(self)
    }

    #[inline(always)]
    fn hash_block(keys: Side<'_>, out: &mut [u32; 64]) {
        #[cfg(all(target_arch = "aarch64", not(miri)))]
        crate::simd::hash64(keys, out);
        #[cfg(not(all(target_arch = "aarch64", not(miri))))]
        {
            let _ = (keys, out);
            unreachable!("no whole-word kernels on this target")
        }
    }

    #[inline(always)]
    fn hash_nulls_block(keys: Side<'_>, non_null: u64, out: &mut [u32; 64]) {
        #[cfg(all(target_arch = "aarch64", not(miri)))]
        crate::simd::hash_nulls64(keys, non_null, out);
        #[cfg(not(all(target_arch = "aarch64", not(miri))))]
        {
            let _ = (keys, non_null, out);
            unreachable!("no whole-word kernels on this target")
        }
    }

    #[inline(always)]
    fn combine_block(keys: Side<'_>, out: &mut [u32; 64]) {
        #[cfg(all(target_arch = "aarch64", not(miri)))]
        crate::simd::combine64(keys, out);
        #[cfg(not(all(target_arch = "aarch64", not(miri))))]
        {
            let _ = (keys, out);
            unreachable!("no whole-word kernels on this target")
        }
    }

    #[inline(always)]
    fn combine_nulls_block(keys: Side<'_>, non_null: u64, out: &mut [u32; 64]) {
        #[cfg(all(target_arch = "aarch64", not(miri)))]
        crate::simd::combine_nulls64(keys, non_null, out);
        #[cfg(not(all(target_arch = "aarch64", not(miri))))]
        {
            let _ = (keys, non_null, out);
            unreachable!("no whole-word kernels on this target")
        }
    }

    #[inline]
    fn add(a: i64, b: i64) -> Result<i64, ArithmeticError> {
        a.checked_add(b).ok_or(ArithmeticError::BigintOutOfRange)
    }

    #[inline]
    fn sub(a: i64, b: i64) -> Result<i64, ArithmeticError> {
        a.checked_sub(b).ok_or(ArithmeticError::BigintOutOfRange)
    }

    #[inline]
    fn mul(a: i64, b: i64) -> Result<i64, ArithmeticError> {
        a.checked_mul(b).ok_or(ArithmeticError::BigintOutOfRange)
    }

    #[inline]
    fn div(a: i64, b: i64) -> Result<i64, ArithmeticError> {
        if b == 0 {
            Err(ArithmeticError::DivisionByZero)
        } else {
            a.checked_div(b).ok_or(ArithmeticError::BigintOutOfRange)
        }
    }

    #[inline]
    fn rem(a: i64, b: i64) -> Result<i64, ArithmeticError> {
        if b == 0 {
            Err(ArithmeticError::DivisionByZero)
        } else {
            Ok(a.wrapping_rem(b))
        }
    }

    #[inline(always)]
    fn divisor(d: i64) -> Option<Divisor> {
        Divisor::new(d)
    }

    /// NEON has no 64-bit multiplication: `*`, a prepared division and a
    /// division by a column go lane by lane from the blocks.
    #[inline(always)]
    fn arith_block<E: Evaluate<i64>>(
        op: ArithOp,
        lhs: Side<'_>,
        rhs: Side<'_>,
        present: u64,
        divisor: &Option<Divisor>,
        out: &mut [MaybeUninit<i64>; 64],
        evaluate: &E,
    ) -> Result<()> {
        #[cfg(all(target_arch = "aarch64", not(miri)))]
        {
            use crate::simd;
            use anyhow::ensure;
            let mut lanes = present;
            match (op, divisor) {
                (ArithOp::Add, _) => {
                    let overflow = simd::add64(lhs, rhs, present, out);
                    ensure!(!overflow, ArithmeticError::BigintOutOfRange);
                }
                (ArithOp::Sub, _) => {
                    let overflow = simd::sub64(lhs, rhs, present, out);
                    ensure!(!overflow, ArithmeticError::BigintOutOfRange);
                }
                (ArithOp::Div, Some(divisor)) => {
                    while lanes != 0 {
                        let lane = lanes.trailing_zeros() as usize;
                        lanes &= lanes - 1;
                        out[lane].write(divisor.quotient(lhs.lane(lane)));
                    }
                }
                (ArithOp::Mod, Some(divisor)) => {
                    while lanes != 0 {
                        let lane = lanes.trailing_zeros() as usize;
                        lanes &= lanes - 1;
                        out[lane].write(divisor.remainder(lhs.lane(lane)));
                    }
                }
                _ => {
                    while lanes != 0 {
                        let lane = lanes.trailing_zeros() as usize;
                        lanes &= lanes - 1;
                        out[lane].write(evaluate(lhs.lane(lane), rhs.lane(lane))?);
                    }
                }
            }
            Ok(())
        }
        #[cfg(not(all(target_arch = "aarch64", not(miri))))]
        {
            let _ = (op, lhs, rhs, present, divisor, out, evaluate);
            unreachable!("no whole-word kernels on this target")
        }
    }
}
