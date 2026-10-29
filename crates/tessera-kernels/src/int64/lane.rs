//! The int8 lane of the drivers of `crate::int`: how a Datum holds an
//! int8, its arithmetic's checks and error, and its vector code of a
//! whole word.

use std::mem::MaybeUninit;

use anyhow::{Result, ensure};
use tessera_core::{WordBlock, ones};

use super::fold;
use super::{CompareOp, Divisor, Side};
use crate::int::{Evaluate, IntLane};
use crate::ops::{ArithOp, ArithmeticError};
use crate::simd::{Neon, Simd, Vectors};

impl IntLane for i64 {
    type Divisor = Divisor;
    type Compares = Simd;
    type Arith = Simd;
    type Aggregates = Simd;
    type Hashes = Neon;

    const ZERO: i64 = 0;
    const ONE: i64 = 1;
    const MIN: i64 = i64::MIN;
    const MAX: i64 = i64::MAX;

    #[inline(always)]
    fn from_datum(word: u64) -> i64 {
        word as i64
    }

    fn side(set: Self::Compares, block: WordBlock<'_, Self>) -> (Side<'_>, u64) {
        match block {
            WordBlock::Dense { values, non_nulls } => (Side::Dense(values), non_nulls),
            WordBlock::Datum { values, isnull } => (Side::Datum(values), set.non_null_bits(isnull)),
        }
    }

    #[inline(always)]
    fn compare_sides(simd: Simd, left: Side<'_>, right: Side<'_>, op: CompareOp) -> u64 {
        crate::simd::compare_sides64(simd, left, right, op)
    }

    #[inline(always)]
    fn filter_block(simd: Simd, block: WordBlock<'_, Self>, scalar: Self, op: CompareOp) -> u64 {
        match block {
            WordBlock::Dense { values, non_nulls } => {
                crate::simd::filter_dense64(simd, values, scalar, op) & non_nulls
            }
            WordBlock::Datum { values, isnull } => {
                crate::simd::filter_datum64(simd, values, isnull, scalar, op)
            }
        }
    }

    #[inline]
    fn min_block(simd: Simd, block: WordBlock<'_, Self>, selected: u64) -> (usize, i64) {
        let mask = crate::int::present(simd, &block, selected);
        let least = match block {
            WordBlock::Dense { values, .. } => crate::simd::min_dense64(simd, values, mask),
            WordBlock::Datum { values, .. } => crate::simd::min_datum64(simd, values, mask),
        };
        (simd.count_bits(mask), least)
    }

    #[inline]
    fn max_block(simd: Simd, block: WordBlock<'_, Self>, selected: u64) -> (usize, i64) {
        let mask = crate::int::present(simd, &block, selected);
        let greatest = match block {
            WordBlock::Dense { values, .. } => crate::simd::max_dense64(simd, values, mask),
            WordBlock::Datum { values, .. } => crate::simd::max_datum64(simd, values, mask),
        };
        (simd.count_bits(mask), greatest)
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
        simd: Simd,
        op: ArithOp,
        lhs: Side<'_>,
        rhs: Side<'_>,
        present: u64,
        divisor: &Option<Divisor>,
        out: &mut [MaybeUninit<i64>; 64],
        evaluate: &E,
    ) -> Result<()> {
        let lanes = present;
        match (op, divisor) {
            (ArithOp::Add, _) => {
                let overflow = crate::simd::add64(simd, lhs, rhs, present, out);
                ensure!(!overflow, ArithmeticError::BigintOutOfRange);
            }
            (ArithOp::Sub, _) => {
                let overflow = crate::simd::sub64(simd, lhs, rhs, present, out);
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
}
