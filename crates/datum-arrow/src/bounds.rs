//! The bounds of a comparison: a PostgreSQL constant and a comparison
//! turned into a test of a column's stored values, in the file's own
//! unit, exact over every value the stored type holds.

use crate::calendar::Count;
use crate::column::{Bitmap, Column, Values, mask_words};
use crate::error::{Error, ErrorKind, Result};
use crate::pg::PgType;
use crate::read::ReadPlan;

/// A comparison of PostgreSQL's B-tree family of a type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Comparison {
    /// `=`.
    Eq,
    /// `<>`, the negator of `=`.
    Ne,
    /// `<`.
    Lt,
    /// `<=`.
    Le,
    /// `>`.
    Gt,
    /// `>=`.
    Ge,
}

/// A test of a column's stored values, in the file's own unit: what a
/// [`ReadPlan::test`] makes of a comparison with a constant.
///
/// A slot that is not `NULL` passes exactly when the comparison of its
/// value with the constant is true, its value being its exact count in
/// PostgreSQL's terms, also where the conversion would refuse it as out
/// of range. A `NULL` slot passes no test.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Test {
    /// Every stored value passes.
    Every,
    /// No stored value passes.
    Nothing,
    /// A stored value passes where `stored op bound` holds, `bound` in
    /// the stored unit; `op` may differ from the comparison asked, where
    /// a bound in a coarser unit needs it.
    Compare {
        /// The comparison of the stored value with the bound.
        op: Comparison,
        /// The bound, in the stored unit.
        bound: i64,
    },
}

/// The answer of a [`Test`] for a closed range of stored values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RangeAnswer {
    /// No value of the range passes.
    NoValue,
    /// Some value of the range may pass: nothing is ruled out.
    SomeMay,
    /// Every value of the range passes.
    EveryValue,
}

impl ReadPlan {
    /// The test of the stored values of the plan's column that keeps
    /// `stored op constant`: `constant` a value of the plan's PostgreSQL
    /// type as a `Datum` gives it, widened to 64 bits, or `None` for a
    /// `NULL` constant.
    ///
    /// For a plan of integers, `constant` is an integer of any of
    /// `smallint`, `integer` and `bigint`; for a plan of days, days since
    /// 2000-01-01, `i32::MAX` and `i32::MIN` being `infinity` and
    /// `-infinity`; for a plan of times, microseconds since 2000-01-01
    /// 00:00:00, `i64::MAX` and `i64::MIN` being the infinities. An
    /// infinity is above every stored value, or below; a `NULL` constant
    /// gives [`Test::Nothing`]. For a plan of a `Dictionary`, the test is
    /// of the dictionary's values, and the caller takes each slot's
    /// answer by its index.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::NotSupported`] for a plan of any other type: a
    /// floating-point number orders `NaN` as PostgreSQL does not, a
    /// string by its collation.
    pub fn test(&self, op: Comparison, constant: Option<i64>) -> Result<Test> {
        let count = self.stored_count().ok_or(Error::new(
            ErrorKind::NotSupported,
            "bounds of a plan that is not of integers, days or times",
        ))?;
        let Some(constant) = constant else {
            return Ok(Test::Nothing);
        };
        let infinity = match self.pg_type() {
            PgType::Date if constant == i64::from(i32::MAX) => Some(true),
            PgType::Date if constant == i64::from(i32::MIN) => Some(false),
            PgType::Timestamp | PgType::Timestamptz if constant == i64::MAX => Some(true),
            PgType::Timestamp | PgType::Timestamptz if constant == i64::MIN => Some(false),
            _ => None,
        };
        if let Some(above) = infinity {
            // Every stored value is below infinity, and above -infinity.
            let below = matches!(op, Comparison::Lt | Comparison::Le | Comparison::Ne);
            let over = matches!(op, Comparison::Gt | Comparison::Ge | Comparison::Ne);
            let passes = if above { below } else { over };
            return Ok(if passes { Test::Every } else { Test::Nothing });
        }
        Ok(bound(count, op, constant))
    }
}

/// `stored * scale op constant + shift`, exact, as a test of `stored`.
fn bound(count: Count, op: Comparison, constant: i64) -> Test {
    let scale = i128::from(count.scale);
    let target = i128::from(constant) + i128::from(count.shift);
    let floor = target.div_euclid(scale);
    let exact = target.rem_euclid(scale) == 0;
    let ceil = if exact { floor } else { floor + 1 };
    let (op, bound) = match op {
        Comparison::Lt => (Comparison::Lt, ceil),
        Comparison::Le => (Comparison::Le, floor),
        Comparison::Gt => (Comparison::Gt, floor),
        Comparison::Ge => (Comparison::Ge, ceil),
        Comparison::Eq if exact => (Comparison::Eq, floor),
        Comparison::Eq => return Test::Nothing,
        Comparison::Ne if exact => (Comparison::Ne, floor),
        Comparison::Ne => return Test::Every,
    };
    let (min, max) = (i128::from(i64::MIN), i128::from(i64::MAX));
    let every = match op {
        Comparison::Lt => bound > max,
        Comparison::Le => bound >= max,
        Comparison::Gt => bound < min,
        Comparison::Ge => bound <= min,
        Comparison::Eq => false,
        Comparison::Ne => !(min..=max).contains(&bound),
    };
    let nothing = match op {
        Comparison::Lt => bound <= min,
        Comparison::Le => bound < min,
        Comparison::Gt => bound >= max,
        Comparison::Ge => bound > max,
        Comparison::Eq => !(min..=max).contains(&bound),
        Comparison::Ne => false,
    };
    if every {
        Test::Every
    } else if nothing {
        Test::Nothing
    } else {
        // Neither: the bound is within an i64.
        Test::Compare {
            op,
            bound: bound as i64,
        }
    }
}

impl Test {
    /// Whether a stored value that is not `NULL` passes.
    #[inline]
    pub const fn passes(self, stored: i64) -> bool {
        match self {
            Test::Every => true,
            Test::Nothing => false,
            Test::Compare { op, bound } => match op {
                Comparison::Eq => stored == bound,
                Comparison::Ne => stored != bound,
                Comparison::Lt => stored < bound,
                Comparison::Le => stored <= bound,
                Comparison::Gt => stored > bound,
                Comparison::Ge => stored >= bound,
            },
        }
    }

    /// Whether no value, some or every value of the closed range of
    /// stored values from `least` to `greatest` passes, of the values
    /// that are not `NULL`: a caller keeps every row of a part of a file
    /// on [`RangeAnswer::EveryValue`] only where the part holds no `NULL`.
    /// A range whose least value is above its greatest, as the
    /// statistics of a damaged file may give, gets
    /// [`RangeAnswer::SomeMay`], which rules nothing out.
    pub const fn range(self, least: i64, greatest: i64) -> RangeAnswer {
        if least > greatest {
            return RangeAnswer::SomeMay;
        }
        let (every, none) = match self {
            Test::Every => (true, false),
            Test::Nothing => (false, true),
            Test::Compare { op, bound } => match op {
                Comparison::Lt => (greatest < bound, least >= bound),
                Comparison::Le => (greatest <= bound, least > bound),
                Comparison::Gt => (least > bound, greatest <= bound),
                Comparison::Ge => (least >= bound, greatest < bound),
                Comparison::Eq => (
                    least == bound && greatest == bound,
                    bound < least || bound > greatest,
                ),
                Comparison::Ne => (
                    bound < least || bound > greatest,
                    least == bound && greatest == bound,
                ),
            },
        };
        if every {
            RangeAnswer::EveryValue
        } else if none {
            RangeAnswer::NoValue
        } else {
            RangeAnswer::SomeMay
        }
    }

    /// Applies the test to a column of stored integers: sets in `passed`,
    /// a mask of 64-bit words, the bit of each slot that is not `NULL` and
    /// passes, and clears every other bit of its first
    /// [`mask_words`] words.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::BrokenShape`], `passed` unchanged, for a column that is
    /// not of integers, values or a bitmap shorter than the column, or
    /// `passed` of fewer than [`mask_words`] words.
    pub fn apply(self, column: &Column<'_>, passed: &mut [u64]) -> Result<()> {
        let len = column.len;
        let broken = |message| Err(Error::new(ErrorKind::BrokenShape, message));
        if passed.len() < mask_words(len) {
            return broken("a mask of the slots that pass shorter than the column");
        }
        if column.nulls.is_some_and(|nulls| !nulls.covers(len)) {
            return broken("a bitmap of NULL values shorter than the column");
        }
        let held = match column.values {
            Values::Int8(v) => v.len() >= len,
            Values::Int16(v) => v.len() >= len,
            Values::Int32(v) => v.len() >= len,
            Values::Int64(v) => v.len() >= len,
            Values::UInt8(v) => v.len() >= len,
            Values::UInt16(v) => v.len() >= len,
            Values::UInt32(v) => v.len() >= len,
            _ => return broken("a test of a column that is not of integers"),
        };
        if !held {
            return broken("buffers shorter than the column");
        }
        let nulls = column.nulls;
        match column.values {
            Values::Int8(v) => fold(&v[..len], nulls, passed, |x| self.passes(i64::from(*x))),
            Values::Int16(v) => fold(&v[..len], nulls, passed, |x| self.passes(i64::from(*x))),
            Values::Int32(v) => fold(&v[..len], nulls, passed, |x| self.passes(i64::from(*x))),
            Values::Int64(v) => fold(&v[..len], nulls, passed, |x| self.passes(*x)),
            Values::UInt8(v) => fold(&v[..len], nulls, passed, |x| self.passes(i64::from(*x))),
            Values::UInt16(v) => fold(&v[..len], nulls, passed, |x| self.passes(i64::from(*x))),
            Values::UInt32(v) => fold(&v[..len], nulls, passed, |x| self.passes(i64::from(*x))),
            _ => {}
        }
        Ok(())
    }
}

/// The bits of the slots that pass, a word of 64 slots at a time, masked
/// by the slots that are not `NULL`.
#[inline]
fn fold<T>(values: &[T], nulls: Option<Bitmap<'_>>, passed: &mut [u64], test: impl Fn(&T) -> bool) {
    let len = values.len();
    for (word, (chunk, out)) in values.chunks(64).zip(passed.iter_mut()).enumerate() {
        let mut bits = 0u64;
        for (i, value) in chunk.iter().enumerate() {
            bits |= u64::from(test(value)) << i;
        }
        let first = word * 64;
        // The loop sets no bit past the chunk.
        let valid = nulls.map_or(u64::MAX, |nulls| nulls.word(first, len));
        *out = bits & valid;
    }
}

#[cfg(test)]
mod tests;
