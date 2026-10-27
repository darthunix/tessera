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
use crate::simd::Neon;

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

    fn side(neon: Neon, block: WordBlock<'_, Self>) -> (Side<'_>, u64) {
        #[cfg(all(target_arch = "aarch64", not(miri)))]
        return match block {
            WordBlock::Dense { values, non_nulls } => (Side::Dense(values), non_nulls),
            WordBlock::Datum { values, isnull } => (
                Side::Datum(values),
                crate::simd::non_null_bits(neon, isnull),
            ),
        };
        #[cfg(not(all(target_arch = "aarch64", not(miri))))]
        {
            let _ = block;
            match neon {}
        }
    }

    #[inline(always)]
    fn compare_sides(neon: Neon, left: Side<'_>, right: Side<'_>, op: CompareOp) -> u64 {
        #[cfg(all(target_arch = "aarch64", not(miri)))]
        return crate::simd::compare_sides64(neon, left, right, op);
        #[cfg(not(all(target_arch = "aarch64", not(miri))))]
        {
            let _ = (left, right, op);
            match neon {}
        }
    }

    #[inline(always)]
    fn filter_block(neon: Neon, block: WordBlock<'_, Self>, scalar: Self, op: CompareOp) -> u64 {
        #[cfg(all(target_arch = "aarch64", not(miri)))]
        return match block {
            WordBlock::Dense { values, non_nulls } => {
                crate::simd::filter_dense64(neon, values, scalar, op) & non_nulls
            }
            WordBlock::Datum { values, isnull } => {
                crate::simd::filter_datum64(neon, values, isnull, scalar, op)
            }
        };
        #[cfg(not(all(target_arch = "aarch64", not(miri))))]
        {
            let _ = (block, scalar, op);
            match neon {}
        }
    }

    #[inline]
    fn min_block(neon: Neon, block: WordBlock<'_, Self>, selected: u64) -> (usize, i64) {
        #[cfg(all(target_arch = "aarch64", not(miri)))]
        {
            let mask = crate::int::present(neon, &block, selected);
            let least = match block {
                WordBlock::Dense { values, .. } => crate::simd::min_dense64(neon, values, mask),
                WordBlock::Datum { values, .. } => crate::simd::min_datum64(neon, values, mask),
            };
            (mask.count_ones() as usize, least)
        }
        #[cfg(not(all(target_arch = "aarch64", not(miri))))]
        {
            let _ = (block, selected);
            match neon {}
        }
    }

    #[inline]
    fn max_block(neon: Neon, block: WordBlock<'_, Self>, selected: u64) -> (usize, i64) {
        #[cfg(all(target_arch = "aarch64", not(miri)))]
        {
            let mask = crate::int::present(neon, &block, selected);
            let greatest = match block {
                WordBlock::Dense { values, .. } => crate::simd::max_dense64(neon, values, mask),
                WordBlock::Datum { values, .. } => crate::simd::max_datum64(neon, values, mask),
            };
            (mask.count_ones() as usize, greatest)
        }
        #[cfg(not(all(target_arch = "aarch64", not(miri))))]
        {
            let _ = (block, selected);
            match neon {}
        }
    }

    #[inline(always)]
    fn key(self) -> u32 {
        fold(self)
    }

    #[inline(always)]
    fn hash_block(neon: Neon, keys: Side<'_>, out: &mut [u32; 64]) {
        #[cfg(all(target_arch = "aarch64", not(miri)))]
        crate::simd::hash64(neon, keys, out);
        #[cfg(not(all(target_arch = "aarch64", not(miri))))]
        {
            let _ = (keys, out);
            match neon {}
        }
    }

    #[inline(always)]
    fn hash_nulls_block(neon: Neon, keys: Side<'_>, non_null: u64, out: &mut [u32; 64]) {
        #[cfg(all(target_arch = "aarch64", not(miri)))]
        crate::simd::hash_nulls64(neon, keys, non_null, out);
        #[cfg(not(all(target_arch = "aarch64", not(miri))))]
        {
            let _ = (keys, non_null, out);
            match neon {}
        }
    }

    #[inline(always)]
    fn combine_block(neon: Neon, keys: Side<'_>, out: &mut [u32; 64]) {
        #[cfg(all(target_arch = "aarch64", not(miri)))]
        crate::simd::combine64(neon, keys, out);
        #[cfg(not(all(target_arch = "aarch64", not(miri))))]
        {
            let _ = (keys, out);
            match neon {}
        }
    }

    #[inline(always)]
    fn combine_nulls_block(neon: Neon, keys: Side<'_>, non_null: u64, out: &mut [u32; 64]) {
        #[cfg(all(target_arch = "aarch64", not(miri)))]
        crate::simd::combine_nulls64(neon, keys, non_null, out);
        #[cfg(not(all(target_arch = "aarch64", not(miri))))]
        {
            let _ = (keys, non_null, out);
            match neon {}
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
        neon: Neon,
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
            use tessera_core::ones;
            let lanes = present;
            match (op, divisor) {
                (ArithOp::Add, _) => {
                    let overflow = simd::add64(neon, lhs, rhs, present, out);
                    ensure!(!overflow, ArithmeticError::BigintOutOfRange);
                }
                (ArithOp::Sub, _) => {
                    let overflow = simd::sub64(neon, lhs, rhs, present, out);
                    ensure!(!overflow, ArithmeticError::BigintOutOfRange);
                }
                (ArithOp::Div, Some(divisor)) => {
                    for lane in ones(lanes) {
                        out[lane].write(divisor.quotient(lhs.lane(lane)));
                    }
                }
                (ArithOp::Mod, Some(divisor)) => {
                    for lane in ones(lanes) {
                        out[lane].write(divisor.remainder(lhs.lane(lane)));
                    }
                }
                _ => {
                    for lane in ones(lanes) {
                        out[lane].write(evaluate(lhs.lane(lane), rhs.lane(lane))?);
                    }
                }
            }
            Ok(())
        }
        #[cfg(not(all(target_arch = "aarch64", not(miri))))]
        {
            let _ = (op, lhs, rhs, present, divisor, out, evaluate);
            match neon {}
        }
    }
}
