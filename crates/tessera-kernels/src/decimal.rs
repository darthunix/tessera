//! Decimals: numeric values of at most 18 digits as an `i64` at their
//! display scale, read from PostgreSQL's stored form of numeric, computed
//! exactly and written back in the form the core's `make_result` writes.
//!
//! The stored form (`utils/adt/numeric.c`) is, after the varlena header, a
//! short header word (0x8000 set: 0x2000 the sign, 0x1F80 the display
//! scale, 0x0040 and 0x003F the weight) or a long one (the sign in 0xC000,
//! the display scale in 0x3FFF, then an `i16` weight), then the digits of
//! base 10000 from the highest, each at 10000^(weight - i); a header of
//! 0xC000 and above is NaN or an infinity. [`Decimal::read`] takes the
//! bytes after the varlena header and refuses what is not a decimal: a
//! special value, a display scale past 18, more than five digit groups, a
//! value of more than 18 digits, digits past the display scale that are
//! not zeros. [`Decimal::write`] writes a whole varlena with its 4-byte
//! header, as `make_result` writes it: digits aligned to the decimal point,
//! leading and trailing zero digits dropped, zero positive at weight 0, the
//! short header.
//!
//! The operations are the core's on those values: [`Decimal::compare`]
//! orders them exactly, + and - keep the larger scale and * the sum of
//! scales (`add_var`, `mul_var`), a cast to an integer rounds half away
//! from zero (`round_var`); a result of more than 18 digits is `None`, for
//! the caller to compute by the core's function. The batch functions
//! ([`filter`], [`compute`], [`to_int4`], [`to_int8`], [`read`]) run them
//! over the selected rows of a [`Source`], with the rows they leave to the
//! caller in a mask of their own. Integers are stored in native byte order;
//! the varlena header is little-endian's, the only order Tessera builds for.

use std::cmp::Ordering;
use std::mem::MaybeUninit;

use anyhow::{Result, ensure};
use tessera_core::{RowMask, RowMaskView};

#[cfg(not(target_endian = "little"))]
compile_error!("the varlena headers here are little-endian's");

/// The most digits of a decimal's value.
pub const DIGITS: u32 = 18;

/// The largest display scale of a decimal read from a numeric; a product's
/// may reach twice that.
pub const MAX_READ_SCALE: u32 = DIGITS;

/// The largest display scale of a decimal.
pub const MAX_SCALE: u32 = 2 * DIGITS;

/// The most bytes [`Decimal::write`] writes, varlena header included.
pub const NUMERIC_MAX: usize = 32;

/// 10^0 through 10^18.
pub const POWERS: [i64; DIGITS as usize + 1] = {
    let mut powers = [1_i64; DIGITS as usize + 1];
    let mut at = 1;
    while at < powers.len() {
        powers[at] = powers[at - 1] * 10;
        at += 1;
    }
    powers
};

/// The bound of a decimal's magnitude, 10^18, excluded.
const LIMIT: i64 = POWERS[DIGITS as usize];

/// A numeric of at most 18 digits: its value at its display scale.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Decimal {
    value: i64,
    scale: u32,
}

impl Decimal {
    /// `value / 10^scale`, or `None` past 18 digits or a scale of 36.
    #[inline]
    pub const fn new(value: i64, scale: u32) -> Option<Self> {
        if value <= -LIMIT || value >= LIMIT || scale > MAX_SCALE {
            return None;
        }
        Some(Self { value, scale })
    }

    /// The value at the display scale.
    #[inline]
    pub const fn value(self) -> i64 {
        self.value
    }

    /// The display scale.
    #[inline]
    pub const fn scale(self) -> u32 {
        self.scale
    }

    /// The decimal of a numeric's bytes after its varlena header, or `None`
    /// for anything else (see the module documentation).
    // Called per row from the batch loops of another crate.
    #[inline(always)]
    pub fn read(data: &[u8]) -> Option<Self> {
        let header = u16::from_ne_bytes(*data.first_chunk::<2>()?);
        if header & 0xC000 == 0xC000 {
            return None; // NaN or an infinity
        }
        let (negative, scale, weight, offset) = if header & 0x8000 != 0 {
            // Short: 0x2000 the sign, 0x1F80 the scale, 0x0040 and 0x003F the weight.
            let magnitude = i32::from(header & 0x003F);
            let weight = if header & 0x0040 != 0 {
                magnitude - 64
            } else {
                magnitude
            };
            (
                header & 0x2000 != 0,
                u32::from((header & 0x1F80) >> 7),
                weight,
                2,
            )
        } else {
            let weight = i16::from_ne_bytes(*data.get(2..)?.first_chunk::<2>()?);
            (
                header & 0xC000 == 0x4000,
                u32::from(header & 0x3FFF),
                i32::from(weight),
                4,
            )
        };
        if scale > MAX_READ_SCALE {
            return None;
        }
        let groups = data[offset..].as_chunks::<2>().0;
        if groups.is_empty() {
            return Some(Self { value: 0, scale });
        }
        // The last digit stands at 10000^(weight - ngroups + 1): to the display scale.
        let exponent = 4 * (weight - groups.len() as i32 + 1) + scale as i32;
        let magnitude = if groups.len() <= 4 {
            // Four groups of up to 65535 stay below 2^64: the digits are
            // checked once, after the loop, without a branch a group.
            let mut value: u64 = 0;
            let mut invalid = false;
            for pair in groups {
                let digit = u16::from_ne_bytes(*pair);
                invalid |= digit >= 10000;
                value = value * 10000 + u64::from(digit);
            }
            if invalid {
                return None;
            }
            rescale(value, exponent)?
        } else {
            read_long(groups, exponent)?
        };
        Some(Self {
            value: if negative {
                -(magnitude as i64)
            } else {
                magnitude as i64
            },
            scale,
        })
    }

    /// Write the numeric of this decimal into `out` as a whole varlena with
    /// a 4-byte header, as `make_result` writes it; the size is returned.
    #[inline]
    pub fn write(self, out: &mut [u8; NUMERIC_MAX]) -> usize {
        // Digits from the lowest: first the fraction's partial group, its
        // part digits padded to four, then whole groups; the lowest
        // (scale + pad) / 4 are the fraction's.
        let mut digits = [0_i16; 16];
        let mut ndigits = 0;
        let part = self.scale % 4;
        let pad = (4 - part) % 4;
        let mut magnitude = self.value.unsigned_abs();
        if part != 0 {
            let unit = POWERS[part as usize] as u64;
            digits[ndigits] = ((magnitude % unit) * POWERS[pad as usize] as u64) as i16;
            ndigits += 1;
            magnitude /= unit;
        }
        while magnitude != 0 {
            digits[ndigits] = (magnitude % 10000) as i16;
            ndigits += 1;
            magnitude /= 10000;
        }
        // A partial group of zeros under nothing else: the value is zero.
        while ndigits > 0 && digits[ndigits - 1] == 0 {
            ndigits -= 1;
        }
        let mut weight = ndigits as i32 - ((self.scale + pad) / 4) as i32 - 1;
        // Trailing zero digits are the lowest: skip them.
        let mut first = 0;
        while first < ndigits && digits[first] == 0 {
            first += 1;
        }
        if first == ndigits {
            ndigits = 0;
            first = 0;
            weight = 0;
        }
        let size = 4 + 2 + (ndigits - first) * 2;
        out[..4].copy_from_slice(&((size as u32) << 2).to_le_bytes());
        let header = 0x8000_u16
            | if self.value < 0 && ndigits > 0 {
                0x2000
            } else {
                0
            }
            | ((self.scale as u16) << 7)
            | if weight < 0 { 0x0040 } else { 0 }
            | (weight as u16 & 0x003F);
        out[4..6].copy_from_slice(&header.to_ne_bytes());
        // The highest digit first.
        for (index, at) in (first..ndigits).rev().enumerate() {
            out[6 + 2 * index..8 + 2 * index].copy_from_slice(&digits[at].to_ne_bytes());
        }
        size
    }

    /// The exact order of two decimals, as `numeric_cmp` orders them.
    #[inline(always)]
    pub fn compare(self, other: Self) -> Ordering {
        if self.scale == other.scale {
            return self.value.cmp(&other.value);
        }
        let (left, right) = aligned(self, other);
        left.cmp(&right)
    }

    /// Both values at the larger scale, or `None` when the rescaled one
    /// passes an `i64`: every other value is below 10^18, so their sum or
    /// difference passes 18 digits too.
    #[inline(always)]
    fn at_larger_scale(self, other: Self) -> Option<(i64, i64, u32)> {
        let scaled = |value: i64, gap: u32| match POWERS.get(gap as usize) {
            Some(&power) => value.checked_mul(power),
            None => (value == 0).then_some(0),
        };
        Some(match self.scale.cmp(&other.scale) {
            Ordering::Equal => (self.value, other.value, self.scale),
            Ordering::Less => (
                scaled(self.value, other.scale - self.scale)?,
                other.value,
                other.scale,
            ),
            Ordering::Greater => (
                self.value,
                scaled(other.value, self.scale - other.scale)?,
                self.scale,
            ),
        })
    }

    /// The sum at the larger scale, or `None` past 18 digits.
    #[inline(always)]
    pub fn checked_add(self, other: Self) -> Option<Self> {
        let (left, right, scale) = self.at_larger_scale(other)?;
        Self::new(left.checked_add(right)?, scale)
    }

    /// The difference at the larger scale, or `None` past 18 digits.
    #[inline(always)]
    pub fn checked_sub(self, other: Self) -> Option<Self> {
        let (left, right, scale) = self.at_larger_scale(other)?;
        Self::new(left.checked_sub(right)?, scale)
    }

    /// The product at the sum of scales, or `None` past 18 digits or a
    /// scale of 36; a product that passes an `i64` passes 18 digits.
    #[inline(always)]
    pub fn checked_mul(self, other: Self) -> Option<Self> {
        Self::new(
            self.value.checked_mul(other.value)?,
            self.scale + other.scale,
        )
    }

    /// The value with the sign changed.
    #[inline]
    pub const fn negate(self) -> Self {
        Self {
            value: -self.value,
            scale: self.scale,
        }
    }

    /// The magnitude.
    #[inline]
    pub const fn abs(self) -> Self {
        Self {
            value: self.value.abs(),
            scale: self.scale,
        }
    }

    /// The nearest integer, a half rounded away from zero; the scales of
    /// money and most quantities divide by constants.
    #[inline(always)]
    pub fn round(self) -> i64 {
        let round_by = |value: i64, unit: i64| {
            let whole = value / unit;
            let rest = value % unit;
            if rest * 2 >= unit {
                whole + 1
            } else if rest * 2 <= -unit {
                whole - 1
            } else {
                whole
            }
        };
        match self.scale {
            0 => self.value,
            1 => round_by(self.value, 10),
            2 => round_by(self.value, 100),
            3 => round_by(self.value, 1000),
            4 => round_by(self.value, 10000),
            scale @ 5..=18 => round_by(self.value, POWERS[scale as usize]),
            // Past 18 places every value is below half a unit.
            _ => 0,
        }
    }
}

/// Two decimals at the larger scale, exactly, or saturated where a gap of
/// scales puts the rescaled one past every value of the other: its sign
/// and magnitude beyond 10^36 order and add as the exact ones would, since
/// the other stays below 10^18.
#[inline]
fn aligned(left: Decimal, right: Decimal) -> (i128, i128) {
    let rescale = |decimal: Decimal, gap: u32| {
        let value = i128::from(decimal.value);
        match POWERS.get(gap as usize) {
            Some(&power) => value * i128::from(power),
            None => value.signum() * i128::from(LIMIT) * i128::from(LIMIT),
        }
    };
    match left.scale.cmp(&right.scale) {
        Ordering::Less => (
            rescale(left, right.scale - left.scale),
            i128::from(right.value),
        ),
        Ordering::Greater => (
            i128::from(left.value),
            rescale(right, left.scale - right.scale),
        ),
        Ordering::Equal => (i128::from(left.value), i128::from(right.value)),
    }
}

/// `value * 10^exponent` below 10^18, or `None`; the digits a negative
/// exponent drops must be zeros. One branch on the sign: a division by a
/// power from the table costs less than a mispredicted choice among
/// constant divisors, and the choice depends on every value.
#[inline(always)]
fn rescale(value: u64, exponent: i32) -> Option<u64> {
    let power = *POWERS.get(exponent.unsigned_abs() as usize)? as u64;
    let result = if exponent >= 0 {
        value.checked_mul(power)?
    } else {
        let quotient = value / power;
        if quotient * power != value {
            return None;
        }
        quotient
    };
    (result < LIMIT as u64).then_some(result)
}

/// The magnitude of five or six digit groups (18 digits and the padding of
/// both end groups take up to six), whose value may pass a `u64` before the
/// display scale drops its lowest digits; more groups are refused.
#[inline(never)]
fn read_long(groups: &[[u8; 2]], exponent: i32) -> Option<u64> {
    if groups.len() > 6 {
        return None;
    }
    let mut value: u128 = 0;
    for pair in groups {
        let digit = u16::from_ne_bytes(*pair);
        if digit >= 10000 {
            return None;
        }
        value = value * 10000 + u128::from(digit);
    }
    if let Ok(value) = u64::try_from(value) {
        return rescale(value, exponent);
    }
    // Past 1.8 * 10^19: only a negative exponent brings it below 10^18.
    let power = *POWERS.get(usize::try_from(-exponent).ok()?)? as u128;
    if !value.is_multiple_of(power) {
        return None;
    }
    u64::try_from(value / power)
        .ok()
        .filter(|&value| value < LIMIT as u64)
}

/// A row's argument as the batch functions read it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Arg {
    /// SQL NULL.
    Null,
    /// A decimal.
    Decimal(Decimal),
    /// A value the core's function must take: NaN, an infinity, a longer
    /// value, a stored form not read in place.
    Other,
}

/// The arguments of a batch function: a column or a scalar.
pub trait Source {
    /// The argument of a row. The batch functions ask only for selected
    /// rows, each at most once a call.
    fn get(&self, row: usize) -> Arg;
}

/// A comparison of [`filter`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Compare {
    /// =
    Eq,
    /// <>
    Ne,
    /// <
    Lt,
    /// <=
    Le,
    /// >
    Gt,
    /// >=
    Ge,
}

impl Compare {
    /// The orders that satisfy the comparison, bit `order + 1` for each of
    /// Less, Equal and Greater.
    #[inline]
    fn orders(self) -> u8 {
        match self {
            Self::Eq => 0b010,
            Self::Ne => 0b101,
            Self::Lt => 0b001,
            Self::Le => 0b011,
            Self::Gt => 0b100,
            Self::Ge => 0b110,
        }
    }
}

/// An operation of [`compute`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Op {
    /// +
    Add,
    /// -
    Sub,
    /// *
    Mul,
    /// Unary -.
    Negate,
    /// abs.
    Abs,
}

impl Op {
    /// Whether the operation takes two arguments.
    #[inline]
    pub const fn binary(self) -> bool {
        matches!(self, Self::Add | Self::Sub | Self::Mul)
    }
}

#[inline]
fn check_rows(nrows: usize, masks: &[usize]) -> Result<()> {
    ensure!(
        masks.iter().all(|&rows| rows == nrows),
        "the masks of a decimal call have different row counts"
    );
    Ok(())
}

/// The selected rows of each word, with the rows the call reads for them.
fn for_each_word(
    rows: RowMaskView<'_>,
    mut visit: impl FnMut(usize, u64) -> Result<()>,
) -> Result<()> {
    for word in 0..rows.nrows().div_ceil(64) {
        visit(word, rows.word(word).unwrap())?;
    }
    Ok(())
}

/// Narrow `rows` to the rows where the comparison of two decimals holds:
/// a row with a NULL leaves, a row where either argument is not a decimal
/// leaves too and is set in `rest`, whose other bits are cleared, for the
/// caller to compare by the core's function.
///
/// # Errors
///
/// Masks of different row counts fail before any mutation.
pub fn filter(
    op: Compare,
    left: &impl Source,
    right: &impl Source,
    rows: &mut RowMask<'_>,
    rest: &mut RowMask<'_>,
) -> Result<()> {
    let nrows = rows.as_view().nrows();
    check_rows(nrows, &[rest.as_view().nrows()])?;
    let orders = op.orders();
    for word in 0..nrows.div_ceil(64) {
        let mut look = rows.as_view().word(word).unwrap();
        let mut keep = 0;
        let mut other = 0;
        while look != 0 {
            let bit = look.trailing_zeros();
            look &= look - 1;
            let row = word * 64 + bit as usize;
            match (left.get(row), right.get(row)) {
                (Arg::Null, _) | (_, Arg::Null) => {}
                (Arg::Decimal(left), Arg::Decimal(right)) => {
                    let order = left.compare(right) as i8 + 1;
                    keep |= u64::from((orders >> order) & 1) << bit;
                }
                _ => other |= 1 << bit,
            }
        }
        rows.set_word(word, keep)?;
        rest.set_word(word, other)?;
    }
    Ok(())
}

/// Where [`compute`] writes a batch's results.
pub struct Results<'a> {
    /// A result's value at its scale, by row.
    pub values: &'a mut [MaybeUninit<i64>],
    /// A result's scale, by row.
    pub scales: &'a mut [MaybeUninit<u8>],
    /// The selected rows without a NULL argument.
    pub non_nulls: RowMask<'a>,
    /// The rows of `non_nulls` whose result the call wrote at the caller's
    /// scale, when it asks for one.
    pub decimals: RowMask<'a>,
    /// The rows of `non_nulls` the call left to the caller: an argument
    /// that is not a decimal, a result past 18 digits.
    pub rest: RowMask<'a>,
}

/// An operation over the selected rows: for each row of `non_nulls` not in
/// `rest`, the exact result's value and scale; a result at `scale`, when
/// given, is also set in `decimals`. Rows outside the selection and rows
/// of `rest` keep unspecified values; every word of the masks is written.
/// `right` is ignored by the unary operations.
///
/// # Errors
///
/// Masks or arrays of different row counts fail before any mutation.
pub fn compute(
    op: Op,
    left: &impl Source,
    right: &impl Source,
    rows: RowMaskView<'_>,
    scale: Option<u32>,
    results: &mut Results<'_>,
) -> Result<()> {
    let nrows = rows.nrows();
    check_rows(
        nrows,
        &[
            results.values.len(),
            results.scales.len(),
            results.non_nulls.as_view().nrows(),
            results.decimals.as_view().nrows(),
            results.rest.as_view().nrows(),
        ],
    )?;
    // A loop for each operation: the choice is made once a call.
    match op {
        Op::Add => compute_with(left, right, rows, scale, results, Decimal::checked_add),
        Op::Sub => compute_with(left, right, rows, scale, results, Decimal::checked_sub),
        Op::Mul => compute_with(left, right, rows, scale, results, Decimal::checked_mul),
        Op::Negate => compute_with(left, &Unary, rows, scale, results, |left, _| {
            Some(left.negate())
        }),
        Op::Abs => compute_with(left, &Unary, rows, scale, results, |left, _| {
            Some(left.abs())
        }),
    }
}

/// The right argument of a unary operation: a placeholder no row reads.
struct Unary;

impl Source for Unary {
    #[inline(always)]
    fn get(&self, _row: usize) -> Arg {
        Arg::Decimal(Decimal { value: 0, scale: 0 })
    }
}

#[inline(always)]
fn compute_with(
    left: &impl Source,
    right: &impl Source,
    rows: RowMaskView<'_>,
    scale: Option<u32>,
    results: &mut Results<'_>,
    apply: impl Fn(Decimal, Decimal) -> Option<Decimal>,
) -> Result<()> {
    for_each_word(rows, |word, mut look| {
        let (mut present, mut decimals, mut other) = (0, 0, 0);
        while look != 0 {
            let bit = look.trailing_zeros();
            look &= look - 1;
            let row = word * 64 + bit as usize;
            let result = match (left.get(row), right.get(row)) {
                (Arg::Null, _) | (_, Arg::Null) => continue,
                (Arg::Decimal(left), Arg::Decimal(right)) => apply(left, right),
                _ => None,
            };
            present |= 1 << bit;
            match result {
                Some(result) => {
                    results.values[row].write(result.value);
                    results.scales[row].write(result.scale as u8);
                    if scale == Some(result.scale) {
                        decimals |= 1 << bit;
                    }
                }
                None => other |= 1 << bit,
            }
        }
        results.non_nulls.set_word(word, present)?;
        results.decimals.set_word(word, decimals)?;
        results.rest.set_word(word, other)
    })
}

/// A cast of decimals to integers over the selected rows, rounded half
/// away from zero: each row of `non_nulls` not in `rest` gets its integer;
/// a row that is not a decimal or whose integer `narrow` refuses goes to
/// `rest`.
fn to_int<T: Copy>(
    source: &impl Source,
    rows: RowMaskView<'_>,
    values: &mut [MaybeUninit<T>],
    non_nulls: &mut RowMask<'_>,
    rest: &mut RowMask<'_>,
    narrow: impl Fn(i64) -> Option<T>,
) -> Result<()> {
    check_rows(
        rows.nrows(),
        &[
            values.len(),
            non_nulls.as_view().nrows(),
            rest.as_view().nrows(),
        ],
    )?;
    for_each_word(rows, |word, mut look| {
        let (mut present, mut other) = (0, 0);
        while look != 0 {
            let bit = look.trailing_zeros();
            look &= look - 1;
            let row = word * 64 + bit as usize;
            match source.get(row) {
                Arg::Null => continue,
                Arg::Decimal(decimal) => match narrow(decimal.round()) {
                    Some(value) => {
                        values[row].write(value);
                    }
                    None => other |= 1 << bit,
                },
                Arg::Other => other |= 1 << bit,
            }
            present |= 1 << bit;
        }
        non_nulls.set_word(word, present)?;
        rest.set_word(word, other)
    })
}

/// `int4(numeric)` over the selected rows: `non_nulls` gets the rows
/// without NULL, `rest` those of them not a decimal or whose integer
/// passes the int4 range, where the core's function raises its error, and
/// each other row its integer, rounded half away from zero.
///
/// # Errors
///
/// Masks or arrays of different row counts fail before any mutation.
pub fn to_int4(
    source: &impl Source,
    rows: RowMaskView<'_>,
    values: &mut [MaybeUninit<i32>],
    non_nulls: &mut RowMask<'_>,
    rest: &mut RowMask<'_>,
) -> Result<()> {
    to_int(source, rows, values, non_nulls, rest, |value| {
        i32::try_from(value).ok()
    })
}

/// `int8(numeric)` over the selected rows, as [`to_int4`]; every
/// decimal's integer fits.
///
/// # Errors
///
/// Masks or arrays of different row counts fail before any mutation.
pub fn to_int8(
    source: &impl Source,
    rows: RowMaskView<'_>,
    values: &mut [MaybeUninit<i64>],
    non_nulls: &mut RowMask<'_>,
    rest: &mut RowMask<'_>,
) -> Result<()> {
    to_int(source, rows, values, non_nulls, rest, Some)
}

/// The bound of a [`Sum`]'s magnitude, 10^36, excluded: a sum stays an
/// exact `i128` with room for any decimal rescaled to its scale.
pub const SUM_BOUND: i128 = LIMIT as i128 * LIMIT as i128;

/// A running sum of decimals as the core's numeric `sum` and `avg` keep
/// it: the value at the largest display scale met, below [`SUM_BOUND`] in
/// magnitude, and the rows added.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Sum {
    /// The value at `scale`.
    pub value: i128,
    /// The largest display scale of the decimals added, at most 18.
    pub scale: u32,
    /// The decimals added.
    pub count: u64,
}

impl Sum {
    /// Add a decimal at the larger of its scale and the sum's, or return
    /// false, the sum unchanged, when the result or the rescaled sum would
    /// reach [`SUM_BOUND`] or the decimal's scale passes 18.
    #[inline(always)]
    pub fn add(&mut self, decimal: Decimal) -> bool {
        let term = i128::from(decimal.value);
        let (value, scale) = if decimal.scale == self.scale {
            (self.value + term, self.scale)
        } else if decimal.scale > self.scale {
            if decimal.scale > MAX_READ_SCALE {
                return false;
            }
            let Some(&factor) = POWERS.get((decimal.scale - self.scale) as usize) else {
                return false;
            };
            let factor = i128::from(factor);
            if self.value.abs() >= SUM_BOUND / factor {
                return false;
            }
            (self.value * factor + term, decimal.scale)
        } else {
            let Some(&factor) = POWERS.get((self.scale - decimal.scale) as usize) else {
                return false;
            };
            (self.value + term * i128::from(factor), self.scale)
        };
        if value.abs() >= SUM_BOUND {
            return false;
        }
        self.value = value;
        self.scale = scale;
        self.count += 1;
        true
    }

    /// Add the sum `value` of `count` values at `scale`, as [`Sum::add`]
    /// adds one: false, the sum unchanged, when the result or the rescaled
    /// sum would reach [`SUM_BOUND`], a term rescaled would overflow, or a
    /// scale passes 18.
    #[inline(always)]
    pub fn add_many(&mut self, value: i128, scale: u32, count: u64) -> bool {
        if scale > MAX_READ_SCALE {
            return false;
        }
        let (sum, scale) = if scale == self.scale {
            (self.value.checked_add(value), self.scale)
        } else if scale > self.scale {
            let Some(&factor) = POWERS.get((scale - self.scale) as usize) else {
                return false;
            };
            let factor = i128::from(factor);
            if self.value.abs() >= SUM_BOUND / factor {
                return false;
            }
            ((self.value * factor).checked_add(value), scale)
        } else {
            let Some(&factor) = POWERS.get((self.scale - scale) as usize) else {
                return false;
            };
            (
                value
                    .checked_mul(i128::from(factor))
                    .and_then(|term| self.value.checked_add(term)),
                self.scale,
            )
        };
        let Some(sum) = sum.filter(|sum| sum.abs() < SUM_BOUND) else {
            return false;
        };
        self.value = sum;
        self.scale = scale;
        self.count += count;
        true
    }
}

/// Add the selected rows' decimals to `total`, NULL rows skipped: a row
/// that is not a decimal, or that [`Sum::add`] refuses, goes to `rest`,
/// whose other bits are cleared, for the caller to add by the core's
/// means; exact sums add in any order.
///
/// # Errors
///
/// Masks of different row counts, or a sum past its bound or of a scale
/// past 18, fail before any mutation.
pub fn sum(
    source: &impl Source,
    rows: RowMaskView<'_>,
    total: &mut Sum,
    rest: &mut RowMask<'_>,
) -> Result<()> {
    check_rows(rows.nrows(), &[rest.as_view().nrows()])?;
    ensure!(
        total.value.abs() < SUM_BOUND && total.scale <= MAX_READ_SCALE,
        "a decimal sum past its bound"
    );
    let mut sum = *total;
    for_each_word(rows, |word, mut look| {
        let mut other = 0;
        while look != 0 {
            let bit = look.trailing_zeros();
            look &= look - 1;
            match source.get(word * 64 + bit as usize) {
                Arg::Null => {}
                Arg::Decimal(decimal) if sum.add(decimal) => {}
                _ => other |= 1 << bit,
            }
        }
        rest.set_word(word, other)
    })?;
    *total = sum;
    Ok(())
}

/// A value of numeric that is not a number, told by its header word.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Special {
    /// NaN (0xC000).
    NaN,
    /// +Infinity (0xD000).
    PositiveInfinity,
    /// -Infinity (0xF000).
    NegativeInfinity,
}

impl Special {
    /// The special value of a numeric's bytes after the varlena header,
    /// `None` for a number or bytes too short for a header.
    #[inline(always)]
    pub fn read(data: &[u8]) -> Option<Self> {
        match u16::from_ne_bytes(*data.first_chunk::<2>()?) {
            0xC000 => Some(Self::NaN),
            0xD000 => Some(Self::PositiveInfinity),
            0xF000 => Some(Self::NegativeInfinity),
            _ => None,
        }
    }
}

/// A row's argument of a sum or an average whose state a record keeps
/// ([`SumState`]).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Term {
    /// SQL NULL: skipped.
    Null,
    /// A decimal, an integer being one at scale 0.
    Decimal(Decimal),
    /// NaN or an infinity, which the state counts apart.
    Special(Special),
    /// A value the caller must add by the core's means: a longer value, a
    /// stored form not read in place.
    Other,
}

/// The rows of a word whose terms a source hands over in bulk, decimals
/// of one scale none of which is NULL.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DecimalWord {
    /// The rows, among those asked for.
    pub rows: u64,
    /// Their scale, at most [`MAX_READ_SCALE`].
    pub scale: u32,
}

/// The terms of a batch by row, as [`Source`] gives arguments. The batch
/// functions ask only for selected rows, each at most once a call.
pub trait Terms {
    /// The term of a row.
    fn term(&self, row: usize) -> Term;

    /// Of the rows `rows` of word `index`, hand those whose terms are
    /// decimals of one scale to `add`, the row's bit and value, in bulk: the
    /// term [`Terms::term`] would give such a row is `Decimal` of that value
    /// and scale. The rows handed and their scale are returned; the other
    /// rows go through [`Terms::term`]. `None` hands over none.
    #[inline(always)]
    fn fold_decimals(
        &self,
        index: usize,
        rows: u64,
        add: impl FnMut(usize, i64),
    ) -> Option<DecimalWord>
    where
        Self: Sized,
    {
        let _ = (index, rows, add);
        None
    }
}

/// A row's partial state of a sum or an average, as a partial grouping
/// hands it to the final one, which merges it ([`SumState::merge`]).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Partial {
    /// SQL NULL, an empty state: skipped.
    Null,
    /// A state to merge.
    State(SumState),
    /// A state the caller must merge by the core's means: one with a
    /// numeric rest, a stored form not read in place.
    Other,
}

/// The partial states of a batch by row. The batch functions ask only for
/// selected rows, each at most once a call.
pub trait Partials {
    /// The partial state of a row.
    ///
    /// # Errors
    ///
    /// A value of another format.
    fn partial(&self, row: usize) -> Result<Partial>;
}

/// The state of a sum or an average of numeric (or of integers, as
/// decimals at scale 0) that a record keeps in [`SumState::WORDS`] words,
/// as the core's `NumericAggState` keeps it without its moving-aggregate
/// fields: the finite values' [`Sum`], and whether NaN, +Infinity and
/// -Infinity were met, which the core counts apart from the finite ones.
/// The words: the sum's `i128` low and high halves, the count, then the
/// scale in bits 0 to 7 with NaN, +Infinity and -Infinity at bits 8, 9
/// and 10. All zeros is the empty state.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SumState {
    /// The finite values added.
    pub sum: Sum,
    /// NaN was met.
    pub nan: bool,
    /// +Infinity was met.
    pub positive_infinity: bool,
    /// -Infinity was met.
    pub negative_infinity: bool,
}

impl SumState {
    /// The words of a state.
    pub const WORDS: usize = 4;

    /// The state of its words.
    #[inline(always)]
    pub fn from_words(words: [u64; Self::WORDS]) -> Self {
        let value = (u128::from(words[1]) << 64 | u128::from(words[0])) as i128;
        Self {
            sum: Sum {
                value,
                scale: (words[3] & 0xFF) as u32,
                count: words[2],
            },
            nan: words[3] & 1 << 8 != 0,
            positive_infinity: words[3] & 1 << 9 != 0,
            negative_infinity: words[3] & 1 << 10 != 0,
        }
    }

    /// The words of the state.
    #[inline(always)]
    pub fn to_words(self) -> [u64; Self::WORDS] {
        let value = self.sum.value as u128;
        [
            value as u64,
            (value >> 64) as u64,
            self.sum.count,
            u64::from(self.sum.scale)
                | u64::from(self.nan) << 8
                | u64::from(self.positive_infinity) << 9
                | u64::from(self.negative_infinity) << 10,
        ]
    }

    /// Add the sum `value` of `count` decimals at `scale` to a state's
    /// words in place, as [`Sum::add_many`] adds it: false, the words
    /// unchanged, when the sum refuses it.
    #[inline(always)]
    pub fn add_many_to(
        words: &mut [u64; Self::WORDS],
        value: i128,
        scale: u32,
        count: u64,
    ) -> bool {
        let mut state = Self::from_words(*words);
        let taken = state.sum.add_many(value, scale, count);
        if taken {
            *words = state.to_words();
        }
        taken
    }

    /// Take a term into a state's words in place, as [`Self::add`] takes
    /// it: a decimal of the sum's scale is added to the words without
    /// decoding the rest of the state.
    #[inline(always)]
    pub fn add_to(words: &mut [u64; Self::WORDS], term: Term) -> bool {
        if let Term::Decimal(decimal) = term
            && u64::from(decimal.scale) == words[3] & 0xFF
        {
            let value = (u128::from(words[1]) << 64 | u128::from(words[0])) as i128
                + i128::from(decimal.value);
            // Below the bound in magnitude, both signs in one comparison.
            if (value + (SUM_BOUND - 1)) as u128 > (2 * (SUM_BOUND - 1)) as u128 {
                return false;
            }
            words[0] = value as u64;
            words[1] = ((value as u128) >> 64) as u64;
            words[2] += 1;
            return true;
        }
        let mut state = Self::from_words(*words);
        let taken = state.add(term);
        if taken {
            *words = state.to_words();
        }
        taken
    }

    /// Merge another state into this one, as the core's combine function
    /// merges two: the other's sum added at the larger of their scales
    /// ([`Sum::add_many`]), NaN and the infinities met in either kept;
    /// false, the state unchanged, when the sum refuses the other's at its
    /// bound.
    #[inline(always)]
    pub fn merge(&mut self, other: Self) -> bool {
        if !self
            .sum
            .add_many(other.sum.value, other.sum.scale, other.sum.count)
        {
            return false;
        }
        self.nan |= other.nan;
        self.positive_infinity |= other.positive_infinity;
        self.negative_infinity |= other.negative_infinity;
        true
    }

    /// Take a term: false, the state unchanged, for one the caller adds by
    /// the core's means (a longer value, or a decimal [`Sum::add`]
    /// refuses); NULL changes nothing.
    #[inline(always)]
    pub fn add(&mut self, term: Term) -> bool {
        match term {
            Term::Null => true,
            Term::Decimal(decimal) => self.sum.add(decimal),
            Term::Special(Special::NaN) => {
                self.nan = true;
                true
            }
            Term::Special(Special::PositiveInfinity) => {
                self.positive_infinity = true;
                true
            }
            Term::Special(Special::NegativeInfinity) => {
                self.negative_infinity = true;
                true
            }
            Term::Other => false,
        }
    }
}

/// The scales [`read`] keeps.
pub enum Scales<'a> {
    /// One scale, the first decimal's when `None`; decimals of other scales
    /// are not taken. The scale taken is left here.
    Uniform(&'a mut Option<u32>),
    /// Every decimal, its scale by row.
    ByRow(&'a mut [MaybeUninit<u8>]),
}

/// Read the selected rows as decimals: a row taken gets its value in
/// `values`, its scale by `scales`, and its bit set in `decimals`; every
/// other selected row (NULL, not a decimal, of another scale) gets its bit
/// cleared and its value left as it was. Bits of rows outside the
/// selection are kept.
///
/// # Errors
///
/// Masks or arrays of different row counts fail before any mutation.
pub fn read(
    source: &impl Source,
    rows: RowMaskView<'_>,
    mut scales: Scales<'_>,
    values: &mut [MaybeUninit<i64>],
    decimals: &mut RowMask<'_>,
) -> Result<()> {
    let by_row = match &scales {
        Scales::Uniform(_) => values.len(),
        Scales::ByRow(scales) => scales.len(),
    };
    check_rows(
        rows.nrows(),
        &[values.len(), by_row, decimals.as_view().nrows()],
    )?;
    // A loop for each way of keeping scales: the choice is made once a call.
    match &mut scales {
        Scales::Uniform(scale) => {
            // A local the loop keeps in a register, written back once.
            let mut taken = **scale;
            let result = read_with(source, rows, values, decimals, |_, decimal| match taken {
                Some(scale) => scale == decimal.scale,
                None => {
                    taken = Some(decimal.scale);
                    true
                }
            });
            **scale = taken;
            result
        }
        Scales::ByRow(scales) => read_with(source, rows, values, decimals, |row, decimal| {
            scales[row].write(decimal.scale as u8);
            true
        }),
    }
}

/// [`read`] with `take` deciding whether a row's decimal is taken.
#[inline(always)]
fn read_with(
    source: &impl Source,
    rows: RowMaskView<'_>,
    values: &mut [MaybeUninit<i64>],
    decimals: &mut RowMask<'_>,
    mut take: impl FnMut(usize, Decimal) -> bool,
) -> Result<()> {
    for_each_word(rows, |word, selected| {
        let mut look = selected;
        let mut found = 0;
        while look != 0 {
            let bit = look.trailing_zeros();
            look &= look - 1;
            let row = word * 64 + bit as usize;
            let Arg::Decimal(decimal) = source.get(row) else {
                continue;
            };
            if take(row, decimal) {
                values[row].write(decimal.value);
                found |= 1 << bit;
            }
        }
        let kept = decimals.as_view().word(word).unwrap() & !selected;
        decimals.set_word(word, kept | found)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// xorshift64*, fixed seed: the same values on every run.
    fn random(state: &mut u64) -> u64 {
        *state ^= *state >> 12;
        *state ^= *state << 25;
        *state ^= *state >> 27;
        state.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// A decimal of up to `digits` digits at a scale up to `max_scale`.
    fn random_decimal(state: &mut u64, max_scale: u32) -> Decimal {
        let digits = (random(state) % 19) as usize;
        let magnitude = (random(state) % POWERS[digits] as u64) as i64;
        let value = if random(state).is_multiple_of(2) {
            magnitude
        } else {
            -magnitude
        };
        Decimal::new(value, (random(state) % u64::from(max_scale + 1)) as u32).unwrap()
    }

    /// The numeric of a decimal the way the core's `make_result` reaches
    /// it, by its text: the digits of each side of the point in groups of
    /// four from the point, zero groups trimmed from both ends.
    fn reference_numeric(decimal: Decimal) -> Vec<u8> {
        let scale = decimal.scale as usize;
        let text = format!(
            "{:0>width$}",
            decimal.value.unsigned_abs(),
            width = scale + 1
        );
        let (whole, fraction) = text.split_at(text.len() - scale);
        let whole = format!("{:0>width$}", whole, width = whole.len().div_ceil(4) * 4);
        let fraction = format!(
            "{:0<width$}",
            fraction,
            width = fraction.len().div_ceil(4) * 4
        );
        let groups: Vec<i16> = whole
            .as_bytes()
            .chunks(4)
            .chain(fraction.as_bytes().chunks(4))
            .map(|group| std::str::from_utf8(group).unwrap().parse().unwrap())
            .collect();
        let mut weight = (whole.len() / 4) as i32 - 1;
        let mut start = 0;
        while start < groups.len() && groups[start] == 0 {
            start += 1;
            weight -= 1;
        }
        let mut end = groups.len();
        while end > start && groups[end - 1] == 0 {
            end -= 1;
        }
        let digits = &groups[start..end];
        let negative = decimal.value < 0 && !digits.is_empty();
        if digits.is_empty() {
            weight = 0;
        }
        let header = 0x8000_u16
            | if negative { 0x2000 } else { 0 }
            | ((decimal.scale as u16) << 7)
            | if weight < 0 { 0x0040 } else { 0 }
            | (weight as u16 & 0x3F);
        let size = 6 + 2 * digits.len();
        let mut bytes = ((size as u32) << 2).to_le_bytes().to_vec();
        bytes.extend(header.to_ne_bytes());
        for digit in digits {
            bytes.extend(digit.to_ne_bytes());
        }
        bytes
    }

    fn written(decimal: Decimal) -> Vec<u8> {
        let mut out = [0; NUMERIC_MAX];
        let size = decimal.write(&mut out);
        out[..size].to_vec()
    }

    /// A numeric's bytes after the header in the long form, which the core
    /// writes for a scale or weight the short one does not hold.
    fn long_form(negative: bool, scale: u16, weight: i16, digits: &[i16]) -> Vec<u8> {
        let mut bytes = (if negative { 0x4000 } else { 0 } | scale)
            .to_ne_bytes()
            .to_vec();
        bytes.extend(weight.to_ne_bytes());
        for digit in digits {
            bytes.extend(digit.to_ne_bytes());
        }
        bytes
    }

    #[test]
    fn writes_as_make_result_and_reads_back() {
        let mut state = 0x9E37_79B9_7F4A_7C15;
        let mut cases: Vec<Decimal> = [
            (0, 0),
            (0, 2),
            (0, 18),
            (1, 0),
            (-1, 0),
            (10000, 0),
            (-10000, 4),
            (123_456, 2),
            (1, 18),
            (-1, 18),
            (LIMIT - 1, 0),
            (-(LIMIT - 1), 18),
            (LIMIT - 1, 9),
            (5, 36),
            (-(LIMIT - 1), 36),
            (100, 1),
            (1_000_000, 5),
        ]
        .into_iter()
        .map(|(value, scale)| Decimal::new(value, scale).unwrap())
        .collect();
        cases.extend((0..200_000).map(|_| random_decimal(&mut state, MAX_SCALE)));
        for decimal in cases {
            let bytes = written(decimal);
            assert_eq!(bytes, reference_numeric(decimal), "{decimal:?}");
            let read = Decimal::read(&bytes[4..]);
            if decimal.scale <= MAX_READ_SCALE {
                assert_eq!(read, Some(decimal), "{decimal:?}");
            } else {
                assert_eq!(read, None, "{decimal:?}");
            }
        }
    }

    #[test]
    fn reads_long_forms_and_refuses_what_is_not_a_decimal() {
        // 12.5 in the long form, with a trailing zero digit written out.
        let bytes = long_form(false, 1, 0, &[12, 5000, 0]);
        assert_eq!(Decimal::read(&bytes), Decimal::new(125, 1));
        assert_eq!(
            Decimal::read(&long_form(true, 3, -1, &[5000])),
            Decimal::new(-500, 3)
        );
        assert_eq!(Decimal::read(&long_form(true, 3, -1, &[5])), None);
        // Digits past the display scale must be zeros.
        assert_eq!(Decimal::read(&long_form(false, 0, -1, &[5])), None);
        assert_eq!(
            Decimal::read(&long_form(false, 0, 0, &[5, 0])),
            Decimal::new(5, 0)
        );
        // Zero digits at any weight: zero at the scale.
        assert_eq!(
            Decimal::read(&long_form(false, 7, 40, &[])),
            Decimal::new(0, 7)
        );
        // NaN, +Infinity, -Infinity.
        for (special, value) in [
            (0xC000_u16, Special::NaN),
            (0xD000, Special::PositiveInfinity),
            (0xF000, Special::NegativeInfinity),
        ] {
            assert_eq!(Decimal::read(&special.to_ne_bytes()), None);
            assert_eq!(Special::read(&special.to_ne_bytes()), Some(value));
        }
        // A number's header, a header of special bits no value has, no header.
        assert_eq!(Special::read(&long_form(false, 7, 40, &[])), None);
        assert_eq!(Special::read(&0xE000_u16.to_ne_bytes()), None);
        assert_eq!(Special::read(&[0xC0]), None);
        // A scale past 18, seven digit groups, 19 digits, a digit out of range.
        assert_eq!(Decimal::read(&long_form(false, 19, 0, &[1])), None);
        assert_eq!(Decimal::read(&long_form(false, 0, 5, &[1; 7])), None);
        assert_eq!(Decimal::read(&long_form(false, 0, 5, &[1; 6])), None);
        assert_eq!(
            Decimal::read(&long_form(false, 9, 2, &[9, 9999, 9999, 9999, 9999, 9000])),
            Decimal::new(LIMIT - 1, 9)
        );
        assert_eq!(
            Decimal::read(&long_form(false, 9, 2, &[9, 9999, 9999, 9999, 9999, 9900])),
            None
        );
        assert_eq!(
            Decimal::read(&long_form(false, 9, 2, &[10, 0, 0, 0, 0, 0])),
            None
        );
        assert_eq!(Decimal::read(&long_form(false, 0, 5, &[1])), None);
        assert_eq!(Decimal::read(&long_form(false, 0, 4, &[100])), None);
        assert_eq!(
            Decimal::read(&long_form(false, 0, 4, &[100, 0, 0, 0, 0])),
            None
        );
        assert_eq!(
            Decimal::read(&long_form(false, 0, 4, &[99, 9999, 9999, 9999, 9999])),
            Decimal::new(LIMIT - 1, 0)
        );
        assert_eq!(
            Decimal::read(&long_form(true, 18, -1, &[9999, 9999, 9999, 9999, 9999])),
            None
        );
        assert_eq!(
            Decimal::read(&long_form(true, 18, -1, &[9999, 9999, 9999, 9999, 9900])),
            Decimal::new(-(LIMIT - 1), 18)
        );
        assert_eq!(
            Decimal::read(&long_form(false, 0, 4, &[1])),
            Decimal::new(POWERS[16], 0)
        );
        assert_eq!(Decimal::read(&long_form(false, 0, 0, &[10000])), None);
        assert_eq!(Decimal::read(&long_form(false, 0, 0, &[-1])), None);
        // Too short for a header.
        assert_eq!(Decimal::read(&[]), None);
        assert_eq!(Decimal::read(&[0]), None);
        assert_eq!(Decimal::read(&[0, 0, 0]), None);
    }

    /// The exact result of an operation over i128, or `None` past 18
    /// digits, by the definitions of `add_var` and `mul_var`.
    fn reference(op: Op, left: Decimal, right: Decimal) -> Option<Decimal> {
        let at = |decimal: Decimal, scale: u32| {
            i128::from(decimal.value) * 10_i128.pow(scale - decimal.scale)
        };
        let scale = left.scale.max(right.scale);
        let (value, scale) = match op {
            Op::Add => (at(left, scale) + at(right, scale), scale),
            Op::Sub => (at(left, scale) - at(right, scale), scale),
            Op::Mul => (
                i128::from(left.value) * i128::from(right.value),
                left.scale + right.scale,
            ),
            Op::Negate => (-i128::from(left.value), left.scale),
            Op::Abs => (i128::from(left.value).abs(), left.scale),
        };
        if value.abs() >= i128::from(LIMIT) || scale > MAX_SCALE {
            return None;
        }
        Decimal::new(value as i64, scale)
    }

    fn apply(op: Op, left: Decimal, right: Decimal) -> Option<Decimal> {
        match op {
            Op::Add => left.checked_add(right),
            Op::Sub => left.checked_sub(right),
            Op::Mul => left.checked_mul(right),
            Op::Negate => Some(left.negate()),
            Op::Abs => Some(left.abs()),
        }
    }

    #[test]
    fn operations_are_exact() {
        let mut state = 0x1234_5678_9ABC_DEF1;
        for _ in 0..200_000 {
            let left = random_decimal(&mut state, MAX_READ_SCALE);
            let right = random_decimal(&mut state, MAX_READ_SCALE);
            let scale = left.scale.max(right.scale);
            let expected = (i128::from(left.value) * 10_i128.pow(scale - left.scale))
                .cmp(&(i128::from(right.value) * 10_i128.pow(scale - right.scale)));
            assert_eq!(left.compare(right), expected, "{left:?} {right:?}");
            for op in [Op::Add, Op::Sub, Op::Mul, Op::Negate, Op::Abs] {
                assert_eq!(
                    apply(op, left, right),
                    reference(op, left, right),
                    "{op:?} {left:?} {right:?}"
                );
            }
        }
    }

    #[test]
    fn wide_gaps_of_scale_order_and_add_exactly() {
        let one = Decimal::new(1, 0).unwrap();
        let tiny = Decimal::new(-(LIMIT - 1), 36).unwrap();
        let small = Decimal::new(LIMIT - 1, 18).unwrap();
        assert_eq!(one.compare(tiny), Ordering::Greater);
        assert_eq!(tiny.compare(one), Ordering::Less);
        assert_eq!(Decimal::new(-1, 0).unwrap().compare(tiny), Ordering::Less);
        assert_eq!(Decimal::new(0, 0).unwrap().compare(tiny), Ordering::Greater);
        assert_eq!(one.checked_add(tiny), None);
        assert_eq!(one.checked_sub(small), Decimal::new(1, 18));
        assert_eq!(Decimal::new(0, 0).unwrap().checked_add(tiny), Some(tiny));
        assert_eq!(small.checked_mul(small), None);
        assert_eq!(
            Decimal::new(3, 18)
                .unwrap()
                .checked_mul(Decimal::new(2, 18).unwrap()),
            Decimal::new(6, 36)
        );
        assert_eq!(
            Decimal::new(1, 19)
                .unwrap()
                .checked_mul(Decimal::new(1, 18).unwrap()),
            None
        );
    }

    #[test]
    fn sums_rescale_and_stop_at_the_bound() {
        let mut sum = Sum::default();
        assert!(sum.add(Decimal::new(15, 1).unwrap()));
        assert!(sum.add(Decimal::new(-3, 0).unwrap()));
        assert!(sum.add(Decimal::new(25, 3).unwrap()));
        // 1.5 - 3 + 0.025 at scale 3.
        assert_eq!((sum.value, sum.scale, sum.count), (-1475, 3, 3));
        let mut big = Sum {
            value: SUM_BOUND - 1,
            scale: 0,
            count: 1,
        };
        assert!(!big.add(Decimal::new(1, 0).unwrap()));
        // Rescaling past the bound is refused, the sum left as it was.
        assert!(!big.add(Decimal::new(-1, 1).unwrap()));
        assert_eq!((big.value, big.scale, big.count), (SUM_BOUND - 1, 0, 1));
        assert!(big.add(Decimal::new(-1, 0).unwrap()));
        let mut wide = Sum {
            value: LIMIT as i128 - 1,
            scale: 0,
            count: 0,
        };
        assert!(wide.add(Decimal::new(LIMIT - 1, 18).unwrap()));
        assert_eq!(
            wide.value,
            (LIMIT as i128 - 1) * LIMIT as i128 + LIMIT as i128 - 1
        );
        assert!(!wide.add(Decimal::new(1, 36).unwrap()));
        // A scale past 18 is refused whatever the sum's scale.
        let mut scaled = Sum {
            value: 5,
            scale: 3,
            count: 1,
        };
        assert!(!scaled.add(Decimal::new(1, 20).unwrap()));
        assert_eq!((scaled.value, scaled.scale), (5, 3));
    }

    #[test]
    fn sum_states_keep_specials_and_round_trip_their_words() {
        let mut state = SumState::default();
        assert_eq!(state.to_words(), [0; SumState::WORDS]);
        assert!(state.add(Term::Null));
        assert!(state.add(Term::Decimal(Decimal::new(-15, 1).unwrap())));
        assert!(state.add(Term::Special(Special::NegativeInfinity)));
        assert!(!state.add(Term::Other));
        assert!(state.add(Term::Special(Special::NaN)));
        assert_eq!(
            (state.sum.value, state.sum.scale, state.sum.count),
            (-15, 1, 1)
        );
        assert!(state.nan && state.negative_infinity && !state.positive_infinity);
        for sample in [
            state,
            SumState {
                sum: Sum {
                    value: -(SUM_BOUND - 1),
                    scale: 18,
                    count: u64::MAX,
                },
                positive_infinity: true,
                ..SumState::default()
            },
        ] {
            assert_eq!(SumState::from_words(sample.to_words()), sample);
        }
        assert_eq!(
            state.to_words()[3],
            1 | 1 << 8 | 1 << 10,
            "scale 1 with NaN and -Infinity"
        );
        // A term of the sum's scale in place: added below the bound of
        // either sign, refused at it, the words unchanged.
        for sign in [1, -1] {
            let near = SumState {
                sum: Sum {
                    value: sign * (SUM_BOUND - 10),
                    scale: 2,
                    count: 3,
                },
                ..SumState::default()
            };
            let mut words = near.to_words();
            assert!(SumState::add_to(
                &mut words,
                Term::Decimal(Decimal::new(sign as i64 * 9, 2).unwrap())
            ));
            assert_eq!(
                SumState::from_words(words).sum.value,
                sign * (SUM_BOUND - 1)
            );
            assert_eq!(SumState::from_words(words).sum.count, 4);
            let before = words;
            assert!(!SumState::add_to(
                &mut words,
                Term::Decimal(Decimal::new(sign as i64, 2).unwrap())
            ));
            assert_eq!(words, before);
        }
    }

    #[test]
    fn rounds_half_away_from_zero() {
        for (value, scale, whole) in [
            (25, 1, 3),
            (-25, 1, -3),
            (24, 1, 2),
            (-24, 1, -2),
            (5, 0, 5),
            (499_999, 6, 0),
            (500_000, 6, 1),
            (-500_000, 6, -1),
            (LIMIT - 1, 18, 1),
            (LIMIT - 1, 0, LIMIT - 1),
            (LIMIT - 1, 19, 0),
            (-(LIMIT - 1), 36, 0),
        ] {
            assert_eq!(
                Decimal::new(value, scale).unwrap().round(),
                whole,
                "{value} {scale}"
            );
        }
    }
}
