//! The calendar of PostgreSQL's dates and timestamps: a date is its days
//! since 2000-01-01 (`i32`), a timestamp its microseconds since then
//! (`i64`), both with an infinity at each end, and an interval its months,
//! days and microseconds.
//!
//! The Julian day routines are the core's own (`date2j`, `j2date`, `j2day`,
//! `date2isoweek`, `date2isoyear`), the same integer arithmetic, so every
//! day from 4713 BC to the last date maps as the core maps it. On them
//! stand the functions of `kernels/date.c`: a date plus or minus days and
//! the days between dates, the casts between date and timestamp,
//! `date_trunc` of a timestamp and of a timestamp's local time, a
//! timestamp plus an interval (months through the calendar, clamped to the
//! month's last day, then days, then microseconds, each checked, as
//! `timestamp_pl_interval` adds them) and the fields of `extract`. Each
//! fails where the core raises, with its error ([`CalendarError`]); an
//! infinite value stays infinite where the core keeps it. Time zones,
//! unit names and the rows the core takes stay with the caller.
//!
//! The batch functions run them over the selected rows of a [`Source`],
//! writing results and the masks of the rows they wrote or left.

use std::fmt;
use std::mem::MaybeUninit;

use anyhow::{Result, ensure};
use tessera_core::{RowMask, RowMaskView};

/// The Julian day of 2000-01-01, a date's zero.
pub const POSTGRES_EPOCH_JDATE: i32 = 2_451_545;
/// The Julian day of 1970-01-01.
pub const UNIX_EPOCH_JDATE: i32 = 2_440_588;
/// Microseconds of a day.
pub const USECS_PER_DAY: i64 = 86_400_000_000;
/// Microseconds of an hour.
pub const USECS_PER_HOUR: i64 = 3_600_000_000;
/// Microseconds of a minute.
pub const USECS_PER_MINUTE: i64 = 60_000_000;
/// Microseconds of a second.
pub const USECS_PER_SEC: i64 = 1_000_000;
/// Seconds of a day.
pub const SECS_PER_DAY: i64 = 86_400;
/// `-infinity` of a date.
pub const DATE_NOBEGIN: i32 = i32::MIN;
/// `infinity` of a date.
pub const DATE_NOEND: i32 = i32::MAX;
/// `-infinity` of a timestamp.
pub const TIMESTAMP_NOBEGIN: i64 = i64::MIN;
/// `infinity` of a timestamp.
pub const TIMESTAMP_NOEND: i64 = i64::MAX;

/// The first Julian day past the dates, `date2j(5874898, 1, 1)`.
const DATE_END_JULIAN: i32 = 2_147_483_494;
/// The first Julian day past the timestamps, `date2j(294277, 1, 1)`.
const TIMESTAMP_END_JULIAN: i32 = 109_203_528;
/// The first timestamp, Julian day 0.
const MIN_TIMESTAMP: i64 = -211_813_488_000_000_000;
/// The first timestamp past the timestamps.
const END_TIMESTAMP: i64 = 9_223_371_331_200_000_000;

/// The days of each month in a common year and in a leap year.
const DAY_TAB: [[i32; 12]; 2] = [
    [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31],
    [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31],
];

/// An error the core raises for a date or a timestamp, SQLSTATE 22008.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CalendarError {
    /// A date past the dates.
    DateOutOfRange,
    /// A date past the timestamps.
    DateOutOfRangeForTimestamp,
    /// The days between dates, one of them infinite.
    CannotSubtractInfinite,
    /// A timestamp past the timestamps.
    TimestampOutOfRange,
    /// An interval whose negation overflows.
    IntervalOutOfRange,
}

impl CalendarError {
    /// The five-character SQLSTATE of the error.
    pub fn sqlstate(self) -> &'static str {
        "22008"
    }
}

impl fmt::Display for CalendarError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::DateOutOfRange => "date out of range",
            Self::DateOutOfRangeForTimestamp => "date out of range for timestamp",
            Self::CannotSubtractInfinite => "cannot subtract infinite dates",
            Self::TimestampOutOfRange => "timestamp out of range",
            Self::IntervalOutOfRange => "interval out of range",
        })
    }
}

impl std::error::Error for CalendarError {}

/// Whether a date lies in the dates, as `IS_VALID_DATE`.
#[inline]
pub const fn is_valid_date(date: i32) -> bool {
    -POSTGRES_EPOCH_JDATE <= date && date < DATE_END_JULIAN - POSTGRES_EPOCH_JDATE
}

/// Whether a timestamp lies in the timestamps, as `IS_VALID_TIMESTAMP`.
#[inline]
pub const fn is_valid_timestamp(timestamp: i64) -> bool {
    MIN_TIMESTAMP <= timestamp && timestamp < END_TIMESTAMP
}

/// Whether the Julian day routines reach a year and month, as
/// `IS_VALID_JULIAN`: from November 4714 BC (year -4713) to May 5874898.
#[inline]
pub const fn is_valid_julian(year: i32, month: i32) -> bool {
    (year > -4713 || (year == -4713 && month >= 11))
        && (year < 5_874_898 || (year == 5_874_898 && month < 6))
}

/// Whether a year is a leap year, 1 BC being year 0.
#[inline]
pub const fn is_leap(year: i32) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

/// The days of a month, 1 to 12.
#[inline]
pub const fn days_in_month(year: i32, month: i32) -> i32 {
    DAY_TAB[is_leap(year) as usize][(month - 1) as usize]
}

/// The Julian day of a date's fields, as `date2j`.
#[inline]
pub const fn date_to_julian(year: i32, month: i32, day: i32) -> i32 {
    let (year, month) = if month > 2 {
        (year.wrapping_add(4800), month + 1)
    } else {
        (year.wrapping_add(4799), month + 13)
    };
    let century = year / 100;
    let mut julian = year.wrapping_mul(365).wrapping_sub(32167);
    julian = julian.wrapping_add(year / 4 - century + century / 4);
    julian.wrapping_add(7834 * month / 256 + day)
}

/// The year, month and day of a Julian day of at least 0, as `j2date`,
/// whose arithmetic is unsigned.
#[inline]
pub const fn julian_to_date(julian: i32) -> (i32, i32, i32) {
    let mut days = (julian as u32).wrapping_add(32044);
    let mut quad = days / 146_097;
    let extra = (days - quad * 146_097) * 4 + 3;
    days += 60 + quad * 3 + extra / 146_097;
    quad = days / 1461;
    days -= quad * 1461;
    let mut y = days * 4 / 1461;
    days = if y != 0 {
        (days + 305) % 365
    } else {
        (days + 306) % 366
    } + 123;
    y += quad * 4;
    let year = y as i32 - 4800;
    let quad = days * 2141 / 65536;
    let day = (days - 7834 * quad / 256) as i32;
    let month = ((quad + 10) % 12 + 1) as i32;
    (year, month, day)
}

/// The day of the week of a Julian day, Sunday 0, as `j2day`.
#[inline]
pub const fn day_of_week(julian: i32) -> i32 {
    let day = (julian.wrapping_add(1)) % 7;
    if day < 0 { day + 7 } else { day }
}

/// The start of the first ISO week of `year`: the Monday on or before its
/// January 4th, as a Julian day.
#[inline]
const fn iso_year_start(year: i32) -> i32 {
    let day4 = date_to_julian(year, 1, 4);
    day4 - day_of_week(day4 - 1)
}

/// The ISO week of a date, as `date2isoweek`.
#[inline]
pub const fn iso_week(year: i32, month: i32, day: i32) -> i32 {
    let julian = date_to_julian(year, month, day);
    let mut start = iso_year_start(year);
    if julian < start {
        start = iso_year_start(year - 1);
    }
    let mut week = (julian - start) / 7 + 1;
    if week >= 52 {
        let next = iso_year_start(year + 1);
        if julian >= next {
            week = (julian - next) / 7 + 1;
        }
    }
    week
}

/// The ISO year of a date, as `date2isoyear`.
#[inline]
pub const fn iso_year(year: i32, month: i32, day: i32) -> i32 {
    let julian = date_to_julian(year, month, day);
    let mut year = year;
    let mut start = iso_year_start(year);
    if julian < start {
        start = iso_year_start(year - 1);
        year -= 1;
    }
    let week = (julian - start) / 7 + 1;
    if week >= 52 && julian >= iso_year_start(year + 1) {
        year += 1;
    }
    year
}

/// A finite timestamp's day and the microseconds into it, the day rounded
/// down.
#[inline]
pub const fn split(timestamp: i64) -> (i64, i64) {
    let mut day = timestamp / USECS_PER_DAY;
    let mut time = timestamp - day * USECS_PER_DAY;
    if time < 0 {
        time += USECS_PER_DAY;
        day -= 1;
    }
    (day, time)
}

/// Whether a date is infinite.
#[inline]
pub const fn date_is_infinite(date: i32) -> bool {
    date == DATE_NOBEGIN || date == DATE_NOEND
}

/// Whether a timestamp is infinite.
#[inline]
pub const fn timestamp_is_infinite(timestamp: i64) -> bool {
    timestamp == TIMESTAMP_NOBEGIN || timestamp == TIMESTAMP_NOEND
}

/// A date plus days, as `date_pli`: an infinite date stays.
#[inline]
pub fn date_plus_days(date: i32, days: i32) -> Result<i32, CalendarError> {
    if date_is_infinite(date) {
        return Ok(date);
    }
    match date.checked_add(days) {
        Some(result) if is_valid_date(result) => Ok(result),
        _ => Err(CalendarError::DateOutOfRange),
    }
}

/// A date minus days, as `date_mii`: minus `i32::MIN` in two steps.
#[inline]
pub fn date_minus_days(date: i32, days: i32) -> Result<i32, CalendarError> {
    match days.checked_neg() {
        Some(days) => date_plus_days(date, days),
        None => date_plus_days(date_plus_days(date, i32::MAX)?, 1),
    }
}

/// The days between two finite dates, as `date_mi`, whose subtraction
/// wraps.
#[inline]
pub fn date_minus_date(left: i32, right: i32) -> Result<i32, CalendarError> {
    if date_is_infinite(left) || date_is_infinite(right) {
        return Err(CalendarError::CannotSubtractInfinite);
    }
    Ok(left.wrapping_sub(right))
}

/// A timestamp's date, as `timestamp_date`: an infinity its date's.
#[inline]
pub const fn timestamp_to_date(timestamp: i64) -> i32 {
    match timestamp {
        TIMESTAMP_NOBEGIN => DATE_NOBEGIN,
        TIMESTAMP_NOEND => DATE_NOEND,
        _ => split(timestamp).0 as i32,
    }
}

/// A date's midnight, as `date2timestamp`: an infinity its timestamp's.
#[inline]
pub fn date_to_timestamp(date: i32) -> Result<i64, CalendarError> {
    match date {
        DATE_NOBEGIN => Ok(TIMESTAMP_NOBEGIN),
        DATE_NOEND => Ok(TIMESTAMP_NOEND),
        _ if date >= TIMESTAMP_END_JULIAN - POSTGRES_EPOCH_JDATE => {
            Err(CalendarError::DateOutOfRangeForTimestamp)
        }
        _ => Ok(i64::from(date) * USECS_PER_DAY),
    }
}

/// A unit of `date_trunc`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Unit {
    /// microseconds
    Microsecond,
    /// milliseconds
    Millisecond,
    /// second
    Second,
    /// minute
    Minute,
    /// hour
    Hour,
    /// day
    Day,
    /// week, from Monday
    Week,
    /// month
    Month,
    /// quarter
    Quarter,
    /// year
    Year,
    /// decade
    Decade,
    /// century
    Century,
    /// millennium
    Millennium,
}

impl Unit {
    /// The microseconds of a unit below a day.
    #[inline]
    const fn size(self) -> Option<i64> {
        match self {
            Self::Microsecond => Some(1),
            Self::Millisecond => Some(1000),
            Self::Second => Some(USECS_PER_SEC),
            Self::Minute => Some(USECS_PER_MINUTE),
            Self::Hour => Some(USECS_PER_HOUR),
            _ => None,
        }
    }
}

/// The year and month that start the period of a unit of a month and up
/// holding them, as `timestamp_trunc` rounds years, 1 BC being year 0.
#[inline]
const fn truncate_fields(unit: Unit, year: i32, month: i32) -> (i32, i32) {
    let year = match unit {
        Unit::Millennium if year > 0 => ((year + 999) / 1000) * 1000 - 999,
        Unit::Millennium => -((999 - (year - 1)) / 1000) * 1000 + 1,
        Unit::Century if year > 0 => ((year + 99) / 100) * 100 - 99,
        Unit::Century => -((99 - (year - 1)) / 100) * 100 + 1,
        Unit::Decade if year > 0 => (year / 10) * 10,
        Unit::Decade => -((8 - (year - 1)) / 10) * 10,
        _ => year,
    };
    let month = match unit {
        Unit::Month => month,
        Unit::Quarter => 3 * ((month - 1) / 3) + 1,
        _ => 1,
    };
    (year, month)
}

/// The Monday on or before a day: 2000-01-01 was a Saturday, 5 days past one.
#[inline]
const fn monday(day: i64) -> i64 {
    day - ((day + 5) % 7 + 7) % 7
}

/// A finite timestamp truncated to a unit, as `timestamp_trunc`, or
/// `None` below the first timestamp.
#[inline(always)]
pub fn truncate(timestamp: i64, unit: Unit) -> Option<i64> {
    let (day, time) = split(timestamp);
    if let Some(size) = unit.size() {
        return Some(day * USECS_PER_DAY + (time / size) * size);
    }
    let day = match unit {
        Unit::Day => day,
        Unit::Week => monday(day),
        _ => {
            let (year, month, _) = julian_to_date((day + i64::from(POSTGRES_EPOCH_JDATE)) as i32);
            let (year, month) = truncate_fields(unit, year, month);
            if !is_valid_julian(year, month) {
                return None;
            }
            i64::from(date_to_julian(year, month, 1) - POSTGRES_EPOCH_JDATE)
        }
    };
    let result = day * USECS_PER_DAY;
    is_valid_timestamp(result).then_some(result)
}

/// A local time truncated to a unit, as `timestamptz_trunc_internal`
/// truncates its fields: a time below a day, or the Julian day of the
/// period's first day, whose midnight the caller finds in its zone.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Truncated {
    /// The truncated local time.
    Time(i64),
    /// The Julian day of the first day.
    Day(i32),
}

/// A finite local time truncated to a unit, or `None` before the Julian
/// days, where the caller leaves the row to the core.
#[inline(always)]
pub fn truncate_local(local: i64, unit: Unit) -> Option<Truncated> {
    let (day, time) = split(local);
    let epoch = i64::from(POSTGRES_EPOCH_JDATE);
    if day + epoch < 0 {
        return None;
    }
    if let Some(size) = unit.size() {
        return Some(Truncated::Time(day * USECS_PER_DAY + (time / size) * size));
    }
    Some(Truncated::Day(match unit {
        Unit::Day => (day + epoch) as i32,
        Unit::Week => {
            let monday = monday(day) + epoch;
            if monday < 0 {
                return None;
            }
            monday as i32
        }
        _ => {
            let (year, month, _) = julian_to_date((day + epoch) as i32);
            let (year, month) = truncate_fields(unit, year, month);
            if !is_valid_julian(year, month) {
                return None;
            }
            date_to_julian(year, month, 1)
        }
    }))
}

/// An interval: months, days and microseconds, as PostgreSQL stores it.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Interval {
    /// Microseconds.
    pub time: i64,
    /// Days.
    pub day: i32,
    /// Months.
    pub month: i32,
}

impl Interval {
    /// `-infinity`.
    pub const NOBEGIN: Self = Self {
        time: i64::MIN,
        day: i32::MIN,
        month: i32::MIN,
    };
    /// `infinity`.
    pub const NOEND: Self = Self {
        time: i64::MAX,
        day: i32::MAX,
        month: i32::MAX,
    };

    /// The interval negated, as `interval_um_internal`, or
    /// [`CalendarError::IntervalOutOfRange`] when a field overflows.
    #[inline]
    pub fn negate(self) -> Result<Self, CalendarError> {
        if self == Self::NOBEGIN {
            return Ok(Self::NOEND);
        }
        if self == Self::NOEND {
            return Ok(Self::NOBEGIN);
        }
        let negated = (|| {
            Some(Self {
                time: self.time.checked_neg()?,
                day: self.day.checked_neg()?,
                month: self.month.checked_neg()?,
            })
        })();
        match negated {
            Some(result) if result != Self::NOBEGIN && result != Self::NOEND => Ok(result),
            _ => Err(CalendarError::IntervalOutOfRange),
        }
    }
}

/// A timestamp plus an interval, as `timestamp_pl_interval`: an infinite
/// interval makes its infinity (opposite infinities out of range), an
/// infinite timestamp stays; months through the calendar, clamped to the
/// month's last day, then days, then microseconds, each checked.
#[inline(always)]
pub fn add_interval(timestamp: i64, span: Interval) -> Result<i64, CalendarError> {
    let out = CalendarError::TimestampOutOfRange;
    if span == Interval::NOBEGIN {
        return if timestamp == TIMESTAMP_NOEND {
            Err(out)
        } else {
            Ok(TIMESTAMP_NOBEGIN)
        };
    }
    if span == Interval::NOEND {
        return if timestamp == TIMESTAMP_NOBEGIN {
            Err(out)
        } else {
            Ok(TIMESTAMP_NOEND)
        };
    }
    if timestamp_is_infinite(timestamp) {
        return Ok(timestamp);
    }
    let epoch = i64::from(POSTGRES_EPOCH_JDATE);
    let mut timestamp = timestamp;
    if span.month != 0 || span.day != 0 {
        // A time of day on Julian day `julian` is a timestamp exactly when
        // the day lies from 0 up to the timestamps' end: the core's check
        // of each step's timestamp.
        let in_range = |julian: i64| (0..i64::from(TIMESTAMP_END_JULIAN)).contains(&julian);
        let (day, time) = split(timestamp);
        let mut julian = day + epoch;
        if span.month != 0 {
            let (mut year, month, mut mday) = julian_to_date(julian as i32);
            let mut month = month.checked_add(span.month).ok_or(out)?;
            if month > 12 {
                year += (month - 1) / 12;
                month = (month - 1) % 12 + 1;
            } else if month < 1 {
                year += month / 12 - 1;
                month = month % 12 + 12;
            }
            mday = mday.min(days_in_month(year, month));
            if !is_valid_julian(year, month) {
                return Err(out);
            }
            julian = i64::from(date_to_julian(year, month, mday));
            if !in_range(julian) {
                return Err(out);
            }
        }
        julian += i64::from(span.day);
        if !in_range(julian) {
            return Err(out);
        }
        timestamp = (julian - epoch) * USECS_PER_DAY + time;
    }
    timestamp
        .checked_add(span.time)
        .filter(|&result| is_valid_timestamp(result))
        .ok_or(out)
}

/// A field of `extract`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Field {
    /// microseconds, of the minute
    Microsecond,
    /// milliseconds, of the minute, with three places
    Millisecond,
    /// seconds, of the minute, with six places
    Second,
    /// minute
    Minute,
    /// hour
    Hour,
    /// day of the month
    Day,
    /// month
    Month,
    /// quarter
    Quarter,
    /// ISO week
    Week,
    /// year, no year 0
    Year,
    /// decade
    Decade,
    /// century
    Century,
    /// millennium
    Millennium,
    /// ISO year
    IsoYear,
    /// day of the week, Sunday 0
    Dow,
    /// ISO day of the week, Sunday 7
    IsoDow,
    /// day of the year
    Doy,
    /// Julian day, of a date
    Julian,
    /// seconds since 1970, of a date
    Epoch,
}

/// The year, month and day of the last day read, for the next row.
#[derive(Clone, Copy, Debug, Default)]
struct DayFields {
    day: Option<i64>,
    year: i32,
    month: i32,
    mday: i32,
}

impl DayFields {
    #[inline]
    fn of(&mut self, day: i64, julian: i32) -> (i32, i32, i32) {
        if self.day != Some(day) {
            (self.year, self.month, self.mday) = julian_to_date(julian);
            self.day = Some(day);
        }
        (self.year, self.month, self.mday)
    }
}

/// The field of a finite day (days since 2000-01-01, its Julian day at
/// least 0) and the microseconds into it, as `extract_date` and
/// `timestamp_part_common` compute it: the value and its scale.
#[inline(always)]
fn extract_with(field: Field, day: i64, time: i64, fields: &mut DayFields) -> (i64, u32) {
    let julian = (day + i64::from(POSTGRES_EPOCH_JDATE)) as i32;
    let value = match field {
        Field::Epoch => (day + i64::from(POSTGRES_EPOCH_JDATE - UNIX_EPOCH_JDATE)) * SECS_PER_DAY,
        Field::Microsecond => time % USECS_PER_MINUTE,
        Field::Millisecond => return (time % USECS_PER_MINUTE, 3),
        Field::Second => return (time % USECS_PER_MINUTE, 6),
        Field::Minute => (time / USECS_PER_MINUTE) % 60,
        Field::Hour => time / USECS_PER_HOUR,
        Field::Julian => i64::from(julian),
        Field::Dow => i64::from(day_of_week(julian)),
        Field::IsoDow => match day_of_week(julian) {
            0 => 7,
            dow => i64::from(dow),
        },
        _ => {
            let (year, month, mday) = fields.of(day, julian);
            i64::from(match field {
                Field::Day => mday,
                Field::Month => month,
                Field::Quarter => (month - 1) / 3 + 1,
                Field::Week => iso_week(year, month, mday),
                // There is no year 0, just 1 BC and 1 AD.
                Field::Year if year > 0 => year,
                Field::Year => year - 1,
                Field::Decade if year >= 0 => year / 10,
                Field::Decade => -((8 - (year - 1)) / 10),
                Field::Century if year > 0 => (year + 99) / 100,
                Field::Century => -((99 - (year - 1)) / 100),
                Field::Millennium if year > 0 => (year + 999) / 1000,
                Field::Millennium => -((999 - (year - 1)) / 1000),
                Field::IsoYear => match iso_year(year, month, mday) {
                    year if year <= 0 => year - 1,
                    year => year,
                },
                _ => julian - date_to_julian(year, 1, 1) + 1,
            })
        }
    };
    (value, 0)
}

/// The field of a finite day and time: its value and scale.
pub fn extract(field: Field, day: i64, time: i64) -> (i64, u32) {
    extract_with(field, day, time, &mut DayFields::default())
}

/// A column or a scalar of a batch function: a row's value, `None` for
/// NULL. The batch functions ask only for selected rows.
pub trait Source<T> {
    /// The value of a row.
    fn get(&self, row: usize) -> Option<T>;
}

#[inline]
fn check_rows(nrows: usize, masks: &[usize]) -> Result<()> {
    ensure!(
        masks.iter().all(|&rows| rows == nrows),
        "the masks of a calendar call have different row counts"
    );
    Ok(())
}

/// Run `apply` over the selected rows whose arguments are not NULL, in the
/// order of the rows, writing each result and `non_nulls`; the first error
/// fails the call, the outputs then unspecified.
#[inline(always)]
fn map_rows<L: Copy, R: Copy, T>(
    left: &impl Source<L>,
    right: &impl Source<R>,
    rows: RowMaskView<'_>,
    values: &mut [MaybeUninit<T>],
    non_nulls: &mut RowMask<'_>,
    apply: impl Fn(L, R) -> Result<T, CalendarError>,
) -> Result<()> {
    check_rows(rows.nrows(), &[values.len(), non_nulls.as_view().nrows()])?;
    for word in 0..rows.nrows().div_ceil(64) {
        let mut look = rows.word(word).unwrap();
        let mut present = 0;
        while look != 0 {
            let bit = look.trailing_zeros();
            look &= look - 1;
            let row = word * 64 + bit as usize;
            let (Some(left), Some(right)) = (left.get(row), right.get(row)) else {
                continue;
            };
            values[row].write(apply(left, right)?);
            present |= 1 << bit;
        }
        non_nulls.set_word(word, present)?;
    }
    Ok(())
}

/// The second argument of a function of one: a placeholder every row has.
struct Unary;

impl Source<()> for Unary {
    #[inline(always)]
    fn get(&self, _row: usize) -> Option<()> {
        Some(())
    }
}

/// An operation of [`date_arith`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DateOp {
    /// date + integer
    PlusDays,
    /// date - integer
    MinusDays,
    /// date - date
    MinusDate,
}

/// date + integer, date - integer or date - date over the selected rows.
///
/// # Errors
///
/// A date past the dates, the difference of an infinite date, or masks and
/// arrays of different row counts.
pub fn date_arith(
    op: DateOp,
    left: &impl Source<i32>,
    right: &impl Source<i32>,
    rows: RowMaskView<'_>,
    values: &mut [MaybeUninit<i32>],
    non_nulls: &mut RowMask<'_>,
) -> Result<()> {
    match op {
        DateOp::PlusDays => map_rows(left, right, rows, values, non_nulls, date_plus_days),
        DateOp::MinusDays => map_rows(left, right, rows, values, non_nulls, date_minus_days),
        DateOp::MinusDate => map_rows(left, right, rows, values, non_nulls, date_minus_date),
    }
}

/// `timestamp(date)` over the selected rows.
///
/// # Errors
///
/// A date past the timestamps, or masks and arrays of different row counts.
pub fn dates_to_timestamps(
    source: &impl Source<i32>,
    rows: RowMaskView<'_>,
    values: &mut [MaybeUninit<i64>],
    non_nulls: &mut RowMask<'_>,
) -> Result<()> {
    map_rows(source, &Unary, rows, values, non_nulls, |date, ()| {
        date_to_timestamp(date)
    })
}

/// `date(timestamp)` over the selected rows.
///
/// # Errors
///
/// Masks and arrays of different row counts.
pub fn timestamps_to_dates(
    source: &impl Source<i64>,
    rows: RowMaskView<'_>,
    values: &mut [MaybeUninit<i32>],
    non_nulls: &mut RowMask<'_>,
) -> Result<()> {
    map_rows(source, &Unary, rows, values, non_nulls, |timestamp, ()| {
        Ok(timestamp_to_date(timestamp))
    })
}

/// `date_trunc(unit, timestamp)` over the selected rows, an infinity kept.
///
/// # Errors
///
/// A truncation below the first timestamp, or masks and arrays of
/// different row counts.
pub fn truncate_timestamps(
    unit: Unit,
    source: &impl Source<i64>,
    rows: RowMaskView<'_>,
    values: &mut [MaybeUninit<i64>],
    non_nulls: &mut RowMask<'_>,
) -> Result<()> {
    map_rows(source, &Unary, rows, values, non_nulls, |timestamp, ()| {
        if timestamp_is_infinite(timestamp) {
            return Ok(timestamp);
        }
        truncate(timestamp, unit).ok_or(CalendarError::TimestampOutOfRange)
    })
}

/// Local times truncated to a unit over the selected rows, each finite
/// one's [`truncate_local`]: a time into `values`, or the Julian day of a
/// first day into `values` with its bit in `days`; an infinite one, or
/// one the routines do not reach, into `rest` for the core. Every word of
/// the masks is written; NULL rows are in none.
///
/// # Errors
///
/// Masks and arrays of different row counts.
pub fn truncate_locals(
    unit: Unit,
    source: &impl Source<i64>,
    rows: RowMaskView<'_>,
    values: &mut [MaybeUninit<i64>],
    days: &mut RowMask<'_>,
    rest: &mut RowMask<'_>,
) -> Result<()> {
    check_rows(
        rows.nrows(),
        &[values.len(), days.as_view().nrows(), rest.as_view().nrows()],
    )?;
    for word in 0..rows.nrows().div_ceil(64) {
        let mut look = rows.word(word).unwrap();
        let (mut day_bits, mut other) = (0, 0);
        while look != 0 {
            let bit = look.trailing_zeros();
            look &= look - 1;
            let row = word * 64 + bit as usize;
            let Some(local) = source.get(row) else {
                continue;
            };
            let truncated = if timestamp_is_infinite(local) {
                None
            } else {
                truncate_local(local, unit)
            };
            match truncated {
                Some(Truncated::Time(time)) => {
                    values[row].write(time);
                }
                Some(Truncated::Day(julian)) => {
                    values[row].write(i64::from(julian));
                    day_bits |= 1 << bit;
                }
                None => other |= 1 << bit,
            }
        }
        days.set_word(word, day_bits)?;
        rest.set_word(word, other)?;
    }
    Ok(())
}

/// A timestamp plus (or minus) an interval over the selected rows.
///
/// # Errors
///
/// A timestamp past the timestamps, an interval whose negation overflows,
/// or masks and arrays of different row counts.
pub fn add_intervals(
    minus: bool,
    left: &impl Source<i64>,
    right: &impl Source<Interval>,
    rows: RowMaskView<'_>,
    values: &mut [MaybeUninit<i64>],
    non_nulls: &mut RowMask<'_>,
) -> Result<()> {
    if minus {
        map_rows(left, right, rows, values, non_nulls, |timestamp, span| {
            add_interval(timestamp, span.negate()?)
        })
    } else {
        map_rows(left, right, rows, values, non_nulls, |timestamp, span| {
            add_interval(timestamp, span)
        })
    }
}

/// A date plus (or minus) an interval over the selected rows, the date
/// its midnight first as `date_pl_interval` makes it, each row's error in
/// the rows' order.
///
/// # Errors
///
/// A date past the timestamps, a timestamp past them, an interval whose
/// negation overflows, or masks and arrays of different row counts.
pub fn add_intervals_to_dates(
    minus: bool,
    left: &impl Source<i32>,
    right: &impl Source<Interval>,
    rows: RowMaskView<'_>,
    values: &mut [MaybeUninit<i64>],
    non_nulls: &mut RowMask<'_>,
) -> Result<()> {
    if minus {
        map_rows(left, right, rows, values, non_nulls, |date, span| {
            add_interval(date_to_timestamp(date)?, span.negate()?)
        })
    } else {
        map_rows(left, right, rows, values, non_nulls, |date, span| {
            add_interval(date_to_timestamp(date)?, span)
        })
    }
}

/// Where the extract functions write a batch's fields.
pub struct Fields<'a> {
    /// A field's value at its scale, by row.
    pub values: &'a mut [MaybeUninit<i64>],
    /// A field's scale, by row.
    pub scales: &'a mut [MaybeUninit<u8>],
    /// The selected rows without NULL.
    pub non_nulls: RowMask<'a>,
    /// The rows of `non_nulls` left to the caller: an infinite value, one
    /// before the Julian days.
    pub rest: RowMask<'a>,
}

/// The fields of the selected rows' days, `day_of` giving a value's day
/// and time or `None` for the rest.
#[inline(always)]
fn extract_rows<T>(
    field: Field,
    source: &impl Source<T>,
    rows: RowMaskView<'_>,
    out: &mut Fields<'_>,
    day_of: impl Fn(T) -> Option<(i64, i64)>,
) -> Result<()> {
    check_rows(
        rows.nrows(),
        &[
            out.values.len(),
            out.scales.len(),
            out.non_nulls.as_view().nrows(),
            out.rest.as_view().nrows(),
        ],
    )?;
    let mut fields = DayFields::default();
    for word in 0..rows.nrows().div_ceil(64) {
        let mut look = rows.word(word).unwrap();
        let (mut present, mut other) = (0, 0);
        while look != 0 {
            let bit = look.trailing_zeros();
            look &= look - 1;
            let row = word * 64 + bit as usize;
            let Some(value) = source.get(row) else {
                continue;
            };
            present |= 1 << bit;
            match day_of(value) {
                Some((day, time)) => {
                    let (value, scale) = extract_with(field, day, time, &mut fields);
                    out.values[row].write(value);
                    out.scales[row].write(scale as u8);
                }
                None => other |= 1 << bit,
            }
        }
        out.non_nulls.set_word(word, present)?;
        out.rest.set_word(word, other)?;
    }
    Ok(())
}

/// `extract(field from date)` over the selected rows: a finite date's
/// field, an infinite one left in `rest`.
///
/// # Errors
///
/// Masks and arrays of different row counts.
pub fn extract_dates(
    field: Field,
    source: &impl Source<i32>,
    rows: RowMaskView<'_>,
    out: &mut Fields<'_>,
) -> Result<()> {
    extract_rows(field, source, rows, out, |date| {
        (!date_is_infinite(date)).then_some((i64::from(date), 0))
    })
}

/// `extract(field from timestamp)` over the selected rows, a timestamp's
/// or a local time's: a finite value's field, an infinite one or one
/// before the Julian days left in `rest`.
///
/// # Errors
///
/// Masks and arrays of different row counts.
pub fn extract_timestamps(
    field: Field,
    source: &impl Source<i64>,
    rows: RowMaskView<'_>,
    out: &mut Fields<'_>,
) -> Result<()> {
    extract_rows(field, source, rows, out, |timestamp| {
        if timestamp_is_infinite(timestamp) {
            return None;
        }
        let (day, time) = split(timestamp);
        (day + i64::from(POSTGRES_EPOCH_JDATE) >= 0).then_some((day, time))
    })
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use proptest::prelude::*;
    use proptest::sample::select;
    use tessera_testing::property;

    use super::*;

    /// Days since 1970-01-01 to the proleptic Gregorian date, by Howard
    /// Hinnant's `civil_from_days`: a reference of another derivation.
    fn civil_from_days(days: i64) -> (i64, i64, i64) {
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        (if m <= 2 { y + 1 } else { y }, m, d)
    }

    fn reference(julian: i32) -> (i32, i32, i32) {
        let (y, m, d) = civil_from_days(i64::from(julian) - i64::from(UNIX_EPOCH_JDATE));
        (y as i32, m as i32, d as i32)
    }

    /// A Julian day of the dates: near an epoch or an end one time in two,
    /// any day otherwise.
    fn julian() -> BoxedStrategy<i32> {
        let marks = [
            0,
            UNIX_EPOCH_JDATE,
            POSTGRES_EPOCH_JDATE,
            TIMESTAMP_END_JULIAN,
            DATE_END_JULIAN - 1,
        ];
        let near = (select(marks.to_vec()), -400_i32..=400)
            .prop_map(|(mark, step)| mark.saturating_add(step).clamp(0, DATE_END_JULIAN - 1));
        prop_oneof![near, 0..DATE_END_JULIAN].boxed()
    }

    fn julian_day_matches_the_calendar(julian: i32) {
        let (year, month, day) = julian_to_date(julian);
        assert_eq!((year, month, day), reference(julian), "{julian}");
        assert_eq!(date_to_julian(year, month, day), julian, "{julian}");
        assert!((1..=days_in_month(year, month)).contains(&day));
    }

    #[test]
    fn julian_days_match_the_calendar_and_round_trip() {
        (0..3_000_000)
            .chain((DATE_END_JULIAN - 2_000_000)..DATE_END_JULIAN)
            .for_each(julian_day_matches_the_calendar);
        property(
            proptest::collection::vec(julian(), 0..256),
            |days| -> Result<()> {
                days.into_iter().for_each(julian_day_matches_the_calendar);
                Ok(())
            },
        );
        assert_eq!(date_to_julian(2000, 1, 1), POSTGRES_EPOCH_JDATE);
        assert_eq!(date_to_julian(1970, 1, 1), UNIX_EPOCH_JDATE);
        assert_eq!(date_to_julian(5_874_898, 1, 1), DATE_END_JULIAN);
        assert_eq!(date_to_julian(294_277, 1, 1), TIMESTAMP_END_JULIAN);
        assert_eq!(julian_to_date(0), (-4713, 11, 24));
        assert!(is_valid_julian(-4713, 11) && !is_valid_julian(-4713, 10));
        assert!(is_valid_julian(5_874_898, 5) && !is_valid_julian(5_874_898, 6));
    }

    /// The ISO year and week of a Julian day by its week's Thursday: the
    /// ISO year is the Thursday's year, the week counts Thursdays.
    fn iso_reference(julian: i32) -> (i32, i32) {
        let dow = (day_of_week(julian) + 6) % 7; // Monday 0
        let thursday = julian - dow + 3;
        let (year, _, _) = julian_to_date(thursday);
        let first = date_to_julian(year, 1, 1);
        (year, (thursday - first) / 7 + 1)
    }

    #[test]
    fn iso_weeks_and_years_count_thursdays() {
        for julian in (0..400_000).chain(2_400_000..2_600_000) {
            let (year, month, day) = julian_to_date(julian);
            let (iso_y, iso_w) = iso_reference(julian);
            assert_eq!(iso_week(year, month, day), iso_w, "{year}-{month}-{day}");
            assert_eq!(iso_year(year, month, day), iso_y, "{year}-{month}-{day}");
        }
        // 2021-01-03 is in week 53 of 2020, 2024-12-30 in week 1 of 2025.
        assert_eq!((iso_week(2021, 1, 3), iso_year(2021, 1, 3)), (53, 2020));
        assert_eq!((iso_week(2024, 12, 30), iso_year(2024, 12, 30)), (1, 2025));
        assert_eq!(day_of_week(POSTGRES_EPOCH_JDATE), 6);
    }

    #[test]
    fn dates_add_subtract_and_cast_as_the_core() {
        assert_eq!(date_plus_days(0, 31), Ok(31));
        assert_eq!(date_plus_days(DATE_NOEND, -5), Ok(DATE_NOEND));
        assert_eq!(
            date_plus_days(DATE_END_JULIAN - POSTGRES_EPOCH_JDATE - 1, 1),
            Err(CalendarError::DateOutOfRange)
        );
        assert_eq!(
            date_plus_days(-POSTGRES_EPOCH_JDATE, -1),
            Err(CalendarError::DateOutOfRange)
        );
        assert_eq!(
            date_minus_days(10, i32::MIN),
            Err(CalendarError::DateOutOfRange)
        );
        assert_eq!(
            date_minus_days(-POSTGRES_EPOCH_JDATE, -5),
            Ok(5 - POSTGRES_EPOCH_JDATE)
        );
        assert_eq!(date_minus_date(100, 40), Ok(60));
        assert_eq!(
            date_minus_date(DATE_NOBEGIN, 0),
            Err(CalendarError::CannotSubtractInfinite)
        );
        assert_eq!(timestamp_to_date(-1), -1);
        assert_eq!(timestamp_to_date(TIMESTAMP_NOEND), DATE_NOEND);
        assert_eq!(date_to_timestamp(-1), Ok(-USECS_PER_DAY));
        assert_eq!(
            date_to_timestamp(TIMESTAMP_END_JULIAN - POSTGRES_EPOCH_JDATE),
            Err(CalendarError::DateOutOfRangeForTimestamp)
        );
        assert_eq!(split(-1), (-1, USECS_PER_DAY - 1));
    }

    /// A timestamp of a date's fields and a time.
    fn at(year: i32, month: i32, day: i32, time: i64) -> i64 {
        i64::from(date_to_julian(year, month, day) - POSTGRES_EPOCH_JDATE) * USECS_PER_DAY + time
    }

    #[test]
    fn truncation_rounds_down_to_the_unit() {
        let t = at(
            2023,
            8,
            17,
            13 * USECS_PER_HOUR + 45 * USECS_PER_MINUTE + 12_345_678,
        );
        for (unit, expected) in [
            (Unit::Microsecond, t),
            (Unit::Millisecond, t - 678),
            (Unit::Second, t - 345_678),
            (Unit::Minute, t - 12_345_678),
            (Unit::Hour, t - 45 * USECS_PER_MINUTE - 12_345_678),
            (Unit::Day, at(2023, 8, 17, 0)),
            (Unit::Week, at(2023, 8, 14, 0)),
            (Unit::Month, at(2023, 8, 1, 0)),
            (Unit::Quarter, at(2023, 7, 1, 0)),
            (Unit::Year, at(2023, 1, 1, 0)),
            (Unit::Decade, at(2020, 1, 1, 0)),
            (Unit::Century, at(2001, 1, 1, 0)),
            (Unit::Millennium, at(2001, 1, 1, 0)),
        ] {
            assert_eq!(truncate(t, unit), Some(expected), "{unit:?}");
        }
        // Before 1 AD, and years of 1 BC as 0.
        let bc = at(-99, 6, 1, 0); // 100 BC
        assert_eq!(truncate(bc, Unit::Century), Some(at(-99, 1, 1, 0)));
        assert_eq!(truncate(bc, Unit::Decade), Some(at(-100, 1, 1, 0)));
        assert_eq!(truncate(bc, Unit::Millennium), Some(at(-999, 1, 1, 0)));
        // The millennium of the first timestamp starts before it.
        assert_eq!(truncate(MIN_TIMESTAMP, Unit::Millennium), None);
        // Julian day 0 was a Monday: its week starts in range.
        assert_eq!(
            truncate(MIN_TIMESTAMP + 3 * USECS_PER_DAY, Unit::Week),
            Some(MIN_TIMESTAMP)
        );
        assert_eq!(
            truncate_local(t, Unit::Month),
            Some(Truncated::Day(date_to_julian(2023, 8, 1)))
        );
        assert_eq!(
            truncate_local(t, Unit::Second),
            Some(Truncated::Time(t - 345_678))
        );
        assert_eq!(truncate_local(MIN_TIMESTAMP - 1, Unit::Day), None);
    }

    #[test]
    fn intervals_add_months_clamped_then_days_then_time() {
        let t = at(2024, 1, 31, 10 * USECS_PER_HOUR);
        let month = Interval {
            time: 0,
            day: 0,
            month: 1,
        };
        assert_eq!(
            add_interval(t, month),
            Ok(at(2024, 2, 29, 10 * USECS_PER_HOUR))
        );
        let back = Interval {
            time: -USECS_PER_HOUR,
            day: -31,
            month: -13,
        };
        assert_eq!(
            add_interval(t, back),
            Ok(at(2022, 11, 30, 9 * USECS_PER_HOUR))
        );
        assert_eq!(add_interval(t, Interval::NOEND), Ok(TIMESTAMP_NOEND));
        assert_eq!(
            add_interval(TIMESTAMP_NOBEGIN, Interval::NOEND),
            Err(CalendarError::TimestampOutOfRange)
        );
        assert_eq!(add_interval(TIMESTAMP_NOEND, month), Ok(TIMESTAMP_NOEND));
        let far = Interval {
            time: 0,
            day: 0,
            month: i32::MAX,
        };
        assert_eq!(
            add_interval(t, far),
            Err(CalendarError::TimestampOutOfRange)
        );
        assert_eq!(Interval::NOBEGIN.negate(), Ok(Interval::NOEND));
        assert_eq!(
            Interval {
                time: 1,
                day: i32::MIN,
                month: 0
            }
            .negate(),
            Err(CalendarError::IntervalOutOfRange)
        );
    }

    /// `timestamp_pl_interval` step by step as the core writes it: each
    /// step's timestamp built and checked.
    fn add_interval_reference(timestamp: i64, span: Interval) -> Result<i64, CalendarError> {
        let out = CalendarError::TimestampOutOfRange;
        if span == Interval::NOBEGIN || span == Interval::NOEND || timestamp_is_infinite(timestamp)
        {
            return add_interval(timestamp, span);
        }
        let epoch = i64::from(POSTGRES_EPOCH_JDATE);
        let mut timestamp = timestamp;
        if span.month != 0 {
            let (day, time) = split(timestamp);
            let (mut year, month, mut mday) = julian_to_date((day + epoch) as i32);
            let mut month = month.checked_add(span.month).ok_or(out)?;
            if month > 12 {
                year += (month - 1) / 12;
                month = (month - 1) % 12 + 1;
            } else if month < 1 {
                year += month / 12 - 1;
                month = month % 12 + 12;
            }
            mday = mday.min(days_in_month(year, month));
            if !is_valid_julian(year, month) {
                return Err(out);
            }
            timestamp = (i64::from(date_to_julian(year, month, mday)) - epoch)
                .checked_mul(USECS_PER_DAY)
                .and_then(|midnight| midnight.checked_add(time))
                .filter(|&result| is_valid_timestamp(result))
                .ok_or(out)?;
        }
        if span.day != 0 {
            let (day, time) = split(timestamp);
            let julian = ((day + epoch) as i32)
                .checked_add(span.day)
                .filter(|&julian| julian >= 0)
                .ok_or(out)?;
            timestamp = (i64::from(julian) - epoch)
                .checked_mul(USECS_PER_DAY)
                .and_then(|midnight| midnight.checked_add(time))
                .filter(|&result| is_valid_timestamp(result))
                .ok_or(out)?;
        }
        timestamp
            .checked_add(span.time)
            .filter(|&result| is_valid_timestamp(result))
            .ok_or(out)
    }

    /// A part of an interval: zero, small, of a middle size or any value.
    fn part<T: Arbitrary + Copy + core::fmt::Debug + From<i8> + 'static>(
        small: impl Strategy<Value = T> + 'static,
        middle: impl Strategy<Value = T> + 'static,
    ) -> BoxedStrategy<T> {
        prop_oneof![Just(T::from(0)), small, middle, any::<T>()].boxed()
    }

    fn intervals() -> impl Strategy<Value = Interval> {
        (
            part(0_i64..1_000_000, -(1_i64 << 39)..(1 << 39)),
            part(-200_i32..200, -50_000_i32..50_000),
            part(-20_i32..20, -5_000_i32..5_000),
        )
            .prop_map(|(time, day, month)| Interval { time, day, month })
    }

    /// A timestamp near either end of the range, within a century of the
    /// epoch, or infinite.
    fn timestamps() -> impl Strategy<Value = i64> {
        prop_oneof![
            4 => (0..1_i64 << 50).prop_map(|offset| MIN_TIMESTAMP + offset),
            4 => (0..1_i64 << 50).prop_map(|offset| END_TIMESTAMP - 1 - offset),
            4 => -(100 * 365 * USECS_PER_DAY)..(100 * 365 * USECS_PER_DAY),
            1 => select(vec![TIMESTAMP_NOBEGIN, TIMESTAMP_NOEND]),
        ]
    }

    #[test]
    fn intervals_add_as_the_core_steps_them() {
        let cases = proptest::collection::vec((timestamps(), intervals()), 0..256);
        property(cases, |cases| -> Result<()> {
            for (timestamp, span) in cases {
                assert_eq!(
                    add_interval(timestamp, span),
                    add_interval_reference(timestamp, span),
                    "{timestamp} {span:?}"
                );
            }
            Ok(())
        });
    }

    #[test]
    fn fields_of_days_and_times() {
        let day = i64::from(date_to_julian(2021, 1, 3) - POSTGRES_EPOCH_JDATE);
        let time = 13 * USECS_PER_HOUR + 7 * USECS_PER_MINUTE + 5_250_000;
        for (field, expected) in [
            (Field::Year, (2021, 0)),
            (Field::Month, (1, 0)),
            (Field::Day, (3, 0)),
            (Field::Quarter, (1, 0)),
            (Field::Week, (53, 0)),
            (Field::IsoYear, (2020, 0)),
            (Field::Dow, (0, 0)),
            (Field::IsoDow, (7, 0)),
            (Field::Doy, (3, 0)),
            (Field::Decade, (202, 0)),
            (Field::Century, (21, 0)),
            (Field::Millennium, (3, 0)),
            (Field::Hour, (13, 0)),
            (Field::Minute, (7, 0)),
            (Field::Second, (5_250_000, 6)),
            (Field::Millisecond, (5_250_000, 3)),
            (Field::Microsecond, (5_250_000, 0)),
            (Field::Julian, (i64::from(date_to_julian(2021, 1, 3)), 0)),
            (Field::Epoch, (1_609_632_000, 0)),
        ] {
            assert_eq!(extract(field, day, time), expected, "{field:?}");
        }
        let bc = i64::from(date_to_julian(-99, 6, 1) - POSTGRES_EPOCH_JDATE);
        assert_eq!(extract(Field::Year, bc, 0), (-100, 0));
        assert_eq!(extract(Field::Century, bc, 0), (-1, 0));
        assert_eq!(extract(Field::Decade, bc, 0), (-10, 0));
        assert_eq!(extract(Field::Millennium, bc, 0), (-1, 0));
    }
}
