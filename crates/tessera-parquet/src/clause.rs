//! The clauses the source checks on a column's stored values: a test
//! made once from a clause and a column's read plan, then applied to the
//! arrays a reader gives, a bit a slot, before any value is made.

use std::cell::RefCell;
use std::fmt;
use std::sync::Arc;

use arrow_array::types::Int32Type;
use arrow_array::{Array, ArrayRef, DictionaryArray};
use datum_arrow::{Comparison, Input, RangeAnswer, ReadPlan, Test, Values, mask_words, oid};
use tessera_kernels::text::Like;

use crate::column::{FileType, MappedColumn};
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
    /// A string equal to one value, its bytes compared.
    BytesEq,
    /// A string unequal to one value.
    BytesNe,
    /// A string that matches a pattern of `LIKE` of fixed parts and `%`.
    Like,
    /// A string that does not match such a pattern.
    NotLike,
}

/// Whether the source checks `LIKE` with `pattern` on a string's bytes: a
/// pattern of fixed parts and `%` signs, without `_` or the escape
/// character, as [`Like::parse`] takes it.
pub fn like_checked(pattern: &[u8]) -> bool {
    Like::parse(pattern).is_some()
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
    /// A string equal to `value`, or unequal where not `equal`.
    Bytes {
        equal: bool,
        value: Vec<u8>,
    },
    /// A string that matches `pattern`, or does not where `negated`; a
    /// pattern [`like_checked`] takes.
    Like {
        pattern: Vec<u8>,
        negated: bool,
    },
    /// No row passes: a clause on strings with `NULL`.
    NoRow,
}

/// The answers of a test for the values of the last dictionary it was
/// applied over: 0 not yet asked, 1 fails, 2 passes. The windows of one
/// chunk share their dictionary's buffers, each window in an array of its
/// own, so the dictionary is known by the addresses of its offsets and
/// bytes and its count of values. The memo holds the array, so that
/// another dictionary cannot take those addresses while it does.
#[derive(Clone, Default)]
struct Memo {
    of: Option<ArrayRef>,
    key: (usize, usize, usize),
    answers: Vec<u8>,
}

impl fmt::Debug for Memo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Memo({} answers)", self.answers.len())
    }
}

/// A clause's test of a column's stored values.
///
/// A test of strings keeps the answers for the values of the last
/// dictionary it was applied over, so that a value of a chunk's dictionary
/// is checked once, however many windows of rows refer to it.
#[derive(Clone, Debug)]
pub struct ClauseTest {
    kind: Kind,
    memo: RefCell<Memo>,
}

/// A test of a string's bytes, borrowed from a test's kind. One lives on
/// the stack for a call: a box of the pattern would allocate a window.
#[allow(clippy::large_enum_variant)]
enum StringTest<'a> {
    Equal(&'a [u8], bool),
    Like(Like<'a>, bool),
}

impl StringTest<'_> {
    fn passes(&self, value: &[u8]) -> bool {
        match self {
            StringTest::Equal(constant, equal) => (value == *constant) == *equal,
            StringTest::Like(like, negated) => like.matches(value) != *negated,
        }
    }
}

impl ClauseTest {
    fn of(kind: Kind) -> Self {
        ClauseTest {
            kind,
            memo: RefCell::default(),
        }
    }

    /// The test of `column`'s strings that keeps the clause `kind`, one of
    /// [`ClauseKind::BytesEq`], [`ClauseKind::BytesNe`],
    /// [`ClauseKind::Like`] and [`ClauseKind::NotLike`], with the bytes of
    /// `value`, `None` for `NULL`: the string compared, or the pattern. A
    /// clause with `NULL` passes no row. The bytes are compared as they
    /// are, as PostgreSQL compares strings under a deterministic
    /// collation.
    ///
    /// # Errors
    ///
    /// `XX000` for another kind, for a column that is not of strings, and
    /// for a pattern [`like_checked`] refuses.
    pub fn of_bytes(column: &MappedColumn, kind: ClauseKind, value: Option<&[u8]>) -> Result<Self> {
        if column.file_type() != FileType::Bytes {
            return Err(broken("a test of strings of a column of another type"));
        }
        let Some(value) = value else {
            return match kind {
                ClauseKind::BytesEq
                | ClauseKind::BytesNe
                | ClauseKind::Like
                | ClauseKind::NotLike => Ok(Self::of(Kind::NoRow)),
                _ => Err(broken("a test of strings of another kind")),
            };
        };
        let kind = match kind {
            ClauseKind::BytesEq | ClauseKind::BytesNe => Kind::Bytes {
                equal: kind == ClauseKind::BytesEq,
                value: value.to_vec(),
            },
            ClauseKind::Like | ClauseKind::NotLike if like_checked(value) => Kind::Like {
                pattern: value.to_vec(),
                negated: kind == ClauseKind::NotLike,
            },
            _ => return Err(broken("a test of strings of another kind or pattern")),
        };
        Ok(Self::of(kind))
    }

    /// The test of a string's bytes of a test of strings.
    fn string_test(&self) -> Option<StringTest<'_>> {
        match &self.kind {
            Kind::Bytes { equal, value } => Some(StringTest::Equal(value, *equal)),
            Kind::Like { pattern, negated } => {
                Like::parse(pattern).map(|like| StringTest::Like(like, *negated))
            }
            _ => None,
        }
    }

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
            ClauseKind::BytesEq | ClauseKind::BytesNe | ClauseKind::Like | ClauseKind::NotLike => {
                return Err(broken("a test of strings without bytes"));
            }
        };
        Ok(Self::of(kind))
    }

    /// Whether group `group` of `file` holds no row that passes, by what
    /// its footer says of `column`'s chunk, the column the test was made
    /// for. Without `dictionary`, by the chunk's statistics alone: its
    /// count of `NULL` values for the tests of `NULL`, and for the other
    /// clauses, which pass no `NULL`, its least and greatest values, as
    /// [`group_stats`] trusts them, of which a string's equality with the
    /// empty string and its inequality take only whether they are empty.
    /// With `dictionary`, by its dictionary, read alone, where the footer
    /// says every data page is encoded through it: ruled out when no value
    /// of it passes; not for `<> ''` where the greatest value is not empty,
    /// which shows that some row is not empty unless a writer gave a
    /// greatest value past the rows. A group the footer does not rule out
    /// is not.
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
        let not_empty =
            matches!(&self.kind, Kind::Bytes { equal: false, value } if value.is_empty());
        if not_empty && stats.empties.is_some_and(|e| !e.greatest_empty) {
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
            // Every order of bytes puts the empty string first: a least
            // value not empty shows no row is, a greatest value empty that
            // every row is. Both at once is a range turned the wrong way,
            // which rules nothing out.
            Kind::Bytes { equal, value } if value.is_empty() => stats
                .empties
                .filter(|e| e.least_empty || !e.greatest_empty)
                .is_some_and(|e| {
                    if *equal {
                        !e.least_empty
                    } else {
                        e.greatest_empty
                    }
                }),
            Kind::Bytes { .. } | Kind::Like { .. } => false,
            Kind::NoRow => true,
        }
    }

    /// Whether no value of a chunk's dictionary passes, every row that is
    /// not `NULL` holding one of them.
    fn rules_out_by(&self, dictionary: &Dictionary) -> bool {
        match (&self.kind, dictionary) {
            (Kind::Compare(test), Dictionary::Integers(values)) => {
                !values.iter().any(|&value| test.passes(value))
            }
            (Kind::In(bounds), Dictionary::Integers(values)) => {
                !values.iter().any(|&value| found(bounds, value))
            }
            (Kind::Bytes { .. } | Kind::Like { .. }, Dictionary::Bytes(values)) => self
                .string_test()
                .is_some_and(|test| !values.iter().any(|value| test.passes(value))),
            _ => false,
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
            Kind::Bytes { .. } | Kind::Like { .. } => {
                let test = self
                    .string_test()
                    .ok_or_else(|| broken("a pattern the reader does not check"))?;
                self.apply_strings(array, &test, &valid, &mut passed[..words])?;
            }
            Kind::NoRow => passed[..words].fill(0),
        }
        for word in &mut passed[words..] {
            *word = 0;
        }
        Ok(())
    }
}

impl ClauseTest {
    /// The bits of `array`'s strings that pass `test`, into `passed`, one
    /// word for each 64 slots; a dictionary's values checked once for all
    /// the windows that refer to them.
    fn apply_strings(
        &self,
        array: &dyn Array,
        test: &StringTest<'_>,
        valid: &[u64],
        passed: &mut [u64],
    ) -> Result<()> {
        if let Some(dictionary) = array.as_any().downcast_ref::<DictionaryArray<Int32Type>>() {
            let values = dictionary.values();
            let strings = Strings::of(values.as_ref())?;
            let mut memo = self.memo.borrow_mut();
            let key = strings.key();
            if memo.of.is_none() || memo.key != key {
                memo.of = Some(Arc::clone(values));
                memo.key = key;
                memo.answers.clear();
                memo.answers.resize(strings.len(), 0);
            }
            let answers = &mut memo.answers;
            let keys = dictionary.keys().values();
            for (word, (chunk, out)) in keys.chunks(64).zip(passed.iter_mut()).enumerate() {
                let mut bits = 0u64;
                let live = valid[word];
                for (i, &key) in chunk.iter().enumerate() {
                    // The key of a NULL slot may be any number.
                    if live >> i & 1 == 0 {
                        continue;
                    }
                    let at = usize::try_from(key).unwrap_or(usize::MAX);
                    let answer = answers
                        .get_mut(at)
                        .ok_or_else(|| broken("a key past its dictionary"))?;
                    if *answer == 0 {
                        *answer = 1 + u8::from(test.passes(strings.get(at)));
                    }
                    bits |= u64::from(*answer == 2) << i;
                }
                *out = bits;
            }
            return Ok(());
        }
        let strings = Strings::of(array)?;
        for (word, out) in passed.iter_mut().enumerate() {
            let mut bits = 0u64;
            for i in 0..64.min(strings.len().saturating_sub(word * 64)) {
                bits |= u64::from(test.passes(strings.get(word * 64 + i))) << i;
            }
            *out = bits & valid[word];
        }
        Ok(())
    }
}

/// The strings of an array of bytes, by their 64-bit offsets: the reader
/// gives every column of bytes so.
struct Strings<'a> {
    offsets: &'a [i64],
    data: &'a [u8],
}

impl<'a> Strings<'a> {
    fn of(array: &'a dyn Array) -> Result<Self> {
        match input_of(array)? {
            Input::Plain(column) => match column.values {
                Values::LargeBytes { offsets, data } => Ok(Strings { offsets, data }),
                _ => Err(broken(
                    "a test of strings of an array that is not of strings",
                )),
            },
            Input::Dictionary { .. } => Err(broken("a dictionary of a dictionary")),
        }
    }

    fn len(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }

    /// The addresses of the offsets and the bytes, and the count of
    /// values, which tell one dictionary from another while it lives.
    fn key(&self) -> (usize, usize, usize) {
        (
            self.offsets.as_ptr().addr(),
            self.data.as_ptr().addr(),
            self.len(),
        )
    }

    /// The bytes of slot `i`, below [`Strings::len`]: Arrow's offsets of a
    /// valid array are in order and within its bytes.
    fn get(&self, i: usize) -> &'a [u8] {
        let from = usize::try_from(self.offsets[i]).unwrap_or(0);
        let to = usize::try_from(self.offsets[i + 1]).unwrap_or(0);
        self.data.get(from..to).unwrap_or_default()
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
