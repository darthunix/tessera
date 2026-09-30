//! The int4 lane of the drivers of `crate::int`: how a Datum holds an
//! int4, its arithmetic's checks and error, and its vector code of a
//! whole word.

use std::mem::MaybeUninit;

use anyhow::{Result, ensure};
use tessera_core::WordBlock;

use super::{CompareOp, Divisor, Side};
use crate::int::{Evaluate, IntLane};
use crate::ops::{ArithOp, ArithmeticError};

impl IntLane for i32 {
    type Divisor = Divisor;

    const ZERO: i32 = 0;
    const ONE: i32 = 1;
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

    #[inline]
    fn add(a: i32, b: i32) -> Result<i32, ArithmeticError> {
        a.checked_add(b).ok_or(ArithmeticError::IntegerOutOfRange)
    }

    #[inline]
    fn sub(a: i32, b: i32) -> Result<i32, ArithmeticError> {
        a.checked_sub(b).ok_or(ArithmeticError::IntegerOutOfRange)
    }

    #[inline]
    fn mul(a: i32, b: i32) -> Result<i32, ArithmeticError> {
        a.checked_mul(b).ok_or(ArithmeticError::IntegerOutOfRange)
    }

    #[inline]
    fn div(a: i32, b: i32) -> Result<i32, ArithmeticError> {
        if b == 0 {
            Err(ArithmeticError::DivisionByZero)
        } else {
            a.checked_div(b).ok_or(ArithmeticError::IntegerOutOfRange)
        }
    }

    #[inline]
    fn rem(a: i32, b: i32) -> Result<i32, ArithmeticError> {
        if b == 0 {
            Err(ArithmeticError::DivisionByZero)
        } else {
            Ok(a.wrapping_rem(b))
        }
    }

    #[inline(always)]
    fn divisor(d: i32) -> Option<Divisor> {
        Divisor::new(d)
    }

    #[inline(always)]
    fn arith_block<E: Evaluate<i32>>(
        op: ArithOp,
        lhs: Side<'_>,
        rhs: Side<'_>,
        present: u64,
        divisor: &Option<Divisor>,
        out: &mut [MaybeUninit<i32>; 64],
        evaluate: &E,
    ) -> Result<()> {
        #[cfg(all(target_arch = "aarch64", not(miri)))]
        {
            use crate::simd;
            // A word without rows to divide divides nothing: a division is
            // too dear to spend on absent lanes, and a whole word of them is
            // a NULL column.
            let overflow = match (op, divisor) {
                (ArithOp::Add, _) => simd::add(lhs, rhs, present, out),
                (ArithOp::Sub, _) => simd::sub(lhs, rhs, present, out),
                (ArithOp::Mul, _) => simd::mul(lhs, rhs, present, out),
                (ArithOp::Div, Some(divisor)) => {
                    if present != 0 {
                        simd::div(lhs, divisor, out);
                    }
                    false
                }
                (ArithOp::Mod, Some(divisor)) => {
                    if present != 0 {
                        simd::rem(lhs, divisor, out);
                    }
                    false
                }
                (ArithOp::Div | ArithOp::Mod, None) => {
                    // No vector division by a column, zero or ±1: the present
                    // lanes one by one from the blocks.
                    let mut lanes = present;
                    while lanes != 0 {
                        let lane = lanes.trailing_zeros() as usize;
                        lanes &= lanes - 1;
                        out[lane].write(evaluate(lhs.lane(lane), rhs.lane(lane))?);
                    }
                    false
                }
            };
            ensure!(!overflow, ArithmeticError::IntegerOutOfRange);
            Ok(())
        }
        #[cfg(not(all(target_arch = "aarch64", not(miri))))]
        {
            let _ = (op, lhs, rhs, present, divisor, out, evaluate);
            unreachable!("no whole-word kernels on this target")
        }
    }
}
