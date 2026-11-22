//! The clauses the source checks on a column's stored values: a test
//! made once from a clause and a column's read plan, then applied to the
//! arrays a reader gives, a bit a slot, before any value is made.

use arrow_array::Array;
use datum_arrow::{Comparison, Input, RangeAnswer, ReadPlan, Test, Values, mask_words, oid};

use crate::column::MappedColumn;
use crate::dictionary::{Dictionary, read_dictionary};
use crate::error::{Error, INTERNAL_ERROR, Result};
use crate::file::ParquetFile;
use crate::reader::input_of;
use crate::stats::{GroupStats, group_stats};

/// What a clause asks of a column's values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClauseKind {
    /// The column compared with one value.
    Compare(Comparison),
    /// The column equal to one of the values: `IN` and `= ANY`.
    In,
    /// `IS NULL`.
    IsNull,
    /// `IS NOT NULL`.
    IsNotNull,
}

/// The strategy numbers of the comparisons of a B-tree family, as
/// PostgreSQL's `stratnum.h` gives them.
const LESS: i32 = 1;
const LESS_EQUAL: i32 = 2;
const EQUAL: i32 = 3;
const GREATER_EQUAL: i32 = 4;
const GREATER: i32 = 5;

/// The comparison of the column with the side that a clause `column op
/// side` asks, or `side op column` where `commuted`, between a column of
/// PostgreSQL type `column` and a side of type `side`. `strategy` is the
/// operator's strategy in the column type's B-tree family, 0 where the
/// operator is not of it, and `negator` that of the operator's negator.
/// `None` for any other clause: a pair of types whose values are not of
/// one unit, the integers' or a date's or a time's own, or an operator
/// that is neither a comparison of the family nor `<>`, the negator of
/// its `=`.
pub fn comparison(
    column: u32,
    side: u32,
    strategy: i32,
    negator: i32,
    commuted: bool,
) -> Option<Comparison> {
    let integer = |of| matches!(of, oid::INT2 | oid::INT4 | oid::INT8);
    let one_unit = match column {
        oid::DATE | oid::TIMESTAMP | oid::TIMESTAMPTZ => side == column,
        _ => integer(column) && integer(side),
    };
    if !one_unit {
        return None;
    }
    // Each comparison, and the same with the side on the left.
    let (op, turned) = match strategy {
        LESS => (Comparison::Lt, Comparison::Gt),
        LESS_EQUAL => (Comparison::Le, Comparison::Ge),
        EQUAL => (Comparison::Eq, Comparison::Eq),
        GREATER_EQUAL => (Comparison::Ge, Comparison::Le),
        GREATER => (Comparison::Gt, Comparison::Lt),
        0 if negator == EQUAL => (Comparison::Ne, Comparison::Ne),
        _ => return None,
    };
    Some(if commuted { turned } else { op })
}

#[derive(Clone, Debug, PartialEq)]
enum Kind {
    Compare(Test),
    /// The bounds in the stored unit that a value may equal, sorted and
    /// without repeats.
    In(Vec<i64>),
    IsNull,
    IsNotNull,
}

/// A clause's test of a column's stored values.
#[derive(Clone, Debug, PartialEq)]
pub struct ClauseTest {
    kind: Kind,
}

impl ClauseTest {
    /// The test of `column`'s stored values that keeps the clause `kind`
    /// with `values`: a value of the column's PostgreSQL type as a `Datum`
    /// gives it, widened to 64 bits, `None` for `NULL`, as
    /// [`datum_arrow::ReadPlan::test`] takes it; one value for a
    /// comparison, any number for `In`, none for the tests of `NULL`. A
    /// comparison with `NULL` passes no row, and a `NULL` in the list of
    /// `In` matches none.
    ///
    /// # Errors
    ///
    /// `XX000` for a comparison not given one value, and for a comparison
    /// or `In` of a column that is not of integers, days or times.
    pub fn new(column: &MappedColumn, kind: ClauseKind, values: &[Option<i64>]) -> Result<Self> {
        let test = |op, value| {
            column
                .plan()
                .test(op, value)
                .map_err(|e| Error::new(INTERNAL_ERROR, e.to_string()))
        };
        let kind = match kind {
            ClauseKind::Compare(op) => match values {
                [value] => Kind::Compare(test(op, *value)?),
                _ => return Err(broken("a comparison of other than one value")),
            },
            ClauseKind::In => Kind::In(
                in_bounds(column.plan(), values)
                    .map_err(|e| Error::new(INTERNAL_ERROR, e.to_string()))?,
            ),
            ClauseKind::IsNull => Kind::IsNull,
            ClauseKind::IsNotNull => Kind::IsNotNull,
        };
        Ok(ClauseTest { kind })
    }

    /// Whether group `group` of `file` holds no row that passes, by what
    /// its footer says of `column`'s chunk, the column the test was made
    /// for. Without `dictionary`, by the chunk's statistics alone: its
    /// count of `NULL` values for the tests of `NULL`, and for the other
    /// clauses, which pass no `NULL`, its least and greatest values, as
    /// [`group_stats`] trusts them. With `dictionary`, by its dictionary,
    /// read alone, where the footer says every data page is encoded
    /// through it: ruled out when no value of it passes. A group the
    /// footer does not rule out is not.
    ///
    /// # Errors
    ///
    /// With `dictionary`, those of [`read_dictionary`].
    pub fn rules_out_group(
        &self,
        file: &ParquetFile,
        column: &MappedColumn,
        group: usize,
        dictionary: bool,
    ) -> Result<bool> {
        let stats = group_stats(file, column, group);
        if !dictionary {
            return Ok(file
                .group_rows(group)
                .is_some_and(|rows| self.rules_out(&stats, rows)));
        }
        if !stats.dictionary_only || matches!(self.kind, Kind::IsNull | Kind::IsNotNull) {
            return Ok(false);
        }
        Ok(read_dictionary(file, column, group)?.is_some_and(|values| self.rules_out_by(&values)))
    }

    /// Whether the statistics `stats` of a chunk of `rows` rows show that
    /// no row passes.
    fn rules_out(&self, stats: &GroupStats, rows: u64) -> bool {
        match &self.kind {
            Kind::IsNull => stats.nulls == Some(0),
            Kind::IsNotNull => stats.nulls == Some(rows),
            Kind::Compare(test) => stats
                .bounds
                .is_some_and(|b| test.range(b.least, b.greatest) == RangeAnswer::NoValue),
            // A range turned the wrong way, as a damaged footer may give,
            // rules nothing out.
            Kind::In(bounds) => stats
                .bounds
                .is_some_and(|b| b.least <= b.greatest && !any_within(bounds, b.least, b.greatest)),
        }
    }

    /// Whether no value of a chunk's dictionary passes, every row that is
    /// not `NULL` holding one of them.
    fn rules_out_by(&self, dictionary: &Dictionary) -> bool {
        let Dictionary::Integers(values) = dictionary else {
            return false;
        };
        match &self.kind {
            Kind::Compare(test) => !values.iter().any(|&value| test.passes(value)),
            Kind::In(bounds) => !values.iter().any(|&value| found(bounds, value)),
            Kind::IsNull | Kind::IsNotNull => false,
        }
    }

    /// Writes into `passed` a bit for each slot of `array`, set where the
    /// slot passes, the bits past the array's slots cleared.
    ///
    /// # Errors
    ///
    /// `XX000` for a mask shorter than the array, and for an array of
    /// another type than the column's plan reads.
    pub fn apply(&self, array: &dyn Array, passed: &mut [u64]) -> Result<()> {
        let len = array.len();
        let words = mask_words(len);
        if passed.len() < words {
            return Err(broken("a mask shorter than its array"));
        }
        let valid = valid_words(array, words);
        match &self.kind {
            Kind::IsNull => {
                for (out, (word, valid)) in passed.iter_mut().zip(valid.iter().enumerate()) {
                    *out = !valid & slots(word, len);
                }
            }
            Kind::IsNotNull => {
                for (out, (word, valid)) in passed.iter_mut().zip(valid.iter().enumerate()) {
                    *out = valid & slots(word, len);
                }
            }
            Kind::Compare(test) => {
                let Input::Plain(column) = input_of(array)? else {
                    return Err(broken("a test of a dictionary of integers"));
                };
                test.apply(&column, &mut passed[..words])
                    .map_err(|e| Error::new(INTERNAL_ERROR, e.to_string()))?;
            }
            Kind::In(bounds) => {
                let Input::Plain(column) = input_of(array)? else {
                    return Err(broken("a test of a dictionary of integers"));
                };
                match column.values {
                    Values::Int8(v) => {
                        fold(&v[..len], &valid, passed, |x| found(bounds, i64::from(*x)))
                    }
                    Values::Int16(v) => {
                        fold(&v[..len], &valid, passed, |x| found(bounds, i64::from(*x)))
                    }
                    Values::Int32(v) => {
                        fold(&v[..len], &valid, passed, |x| found(bounds, i64::from(*x)))
                    }
                    Values::Int64(v) => fold(&v[..len], &valid, passed, |x| found(bounds, *x)),
                    Values::UInt8(v) => {
                        fold(&v[..len], &valid, passed, |x| found(bounds, i64::from(*x)))
                    }
                    Values::UInt16(v) => {
                        fold(&v[..len], &valid, passed, |x| found(bounds, i64::from(*x)))
                    }
                    Values::UInt32(v) => {
                        fold(&v[..len], &valid, passed, |x| found(bounds, i64::from(*x)))
                    }
                    _ => return Err(broken("a test of a column that is not of integers")),
                }
            }
        }
        for word in &mut passed[words..] {
            *word = 0;
        }
        Ok(())
    }
}

/// The bounds in the stored unit of `plan` that a value equal to one of
/// `values` holds, sorted and without repeats.
fn in_bounds(plan: &ReadPlan, values: &[Option<i64>]) -> datum_arrow::Result<Vec<i64>> {
    let mut bounds = equalities(plan, values)?;
    bounds.sort_unstable();
    bounds.dedup();
    Ok(bounds)
}

/// The bound of each value's test of equality, in the values' order,
/// those that match no stored value left out.
fn equalities(plan: &ReadPlan, values: &[Option<i64>]) -> datum_arrow::Result<Vec<i64>> {
    let mut bounds = Vec::with_capacity(values.len());
    for value in values {
        // Every is not the answer of an equality; Nothing matches no row.
        if let Test::Compare { bound, .. } = plan.test(Comparison::Eq, *value)? {
            bounds.push(bound);
        }
    }
    Ok(bounds)
}

/// Whether one of the sorted `bounds` lies from `least` to `greatest`.
fn any_within(bounds: &[i64], least: i64, greatest: i64) -> bool {
    let first = bounds.partition_point(|&bound| bound < least);
    bounds.get(first).is_some_and(|&bound| bound <= greatest)
}

/// Whether `value` is one of the sorted `bounds`.
fn found(bounds: &[i64], value: i64) -> bool {
    bounds.binary_search(&value).is_ok()
}

/// The bits of the slots of word `word` among `len` slots.
fn slots(word: usize, len: usize) -> u64 {
    match len.saturating_sub(word * 64) {
        count if count >= 64 => u64::MAX,
        count => (1u64 << count) - 1,
    }
}

/// A word of bits for each 64 slots of `array`, set where the slot holds
/// a value.
fn valid_words(array: &dyn Array, words: usize) -> Vec<u64> {
    match array.nulls() {
        Some(nulls) => {
            let mut out: Vec<u64> = nulls.inner().bit_chunks().iter_padded().collect();
            out.resize(words, 0);
            out
        }
        None => vec![u64::MAX; words],
    }
}

fn fold<T>(values: &[T], valid: &[u64], passed: &mut [u64], test: impl Fn(&T) -> bool) {
    for (word, (chunk, out)) in values.chunks(64).zip(passed.iter_mut()).enumerate() {
        let mut bits = 0u64;
        for (i, value) in chunk.iter().enumerate() {
            bits |= u64::from(test(value)) << i;
        }
        *out = bits & valid[word];
    }
}

fn broken(message: &str) -> Error {
    Error::new(INTERNAL_ERROR, message)
}

#[cfg(kani)]
mod proofs;
#[cfg(test)]
mod tests;
