//! PostgreSQL's calendar: its epochs, its ranges of dates and times, and
//! the bounds of those ranges in a file's units.
//!
//! PostgreSQL counts a `date` in days since 2000-01-01 and a `timestamp`
//! in microseconds since 2000-01-01 00:00:00; Arrow and bare integers
//! count from 1970-01-01. The constants are those of PostgreSQL's
//! `datatype/timestamp.h` and `utils/date.h`.

/// Days from 1970-01-01 to 2000-01-01: `POSTGRES_EPOCH_JDATE -
/// UNIX_EPOCH_JDATE`.
pub(crate) const EPOCH_DAYS: i64 = 2_451_545 - 2_440_588;

/// Microseconds in a day.
pub(crate) const USECS_PER_DAY: i64 = 86_400_000_000;

/// Microseconds from 1970-01-01 00:00:00 to 2000-01-01 00:00:00.
pub(crate) const EPOCH_USECS: i64 = EPOCH_DAYS * USECS_PER_DAY;

/// The first day PostgreSQL holds, since 2000-01-01:
/// `DATETIME_MIN_JULIAN - POSTGRES_EPOCH_JDATE`.
pub(crate) const MIN_DATE: i64 = -2_451_545;

/// The last day PostgreSQL holds, since 2000-01-01: one before
/// `DATE_END_JULIAN - POSTGRES_EPOCH_JDATE`.
pub(crate) const MAX_DATE: i64 = 2_147_483_494 - 2_451_545 - 1;

/// The first microsecond PostgreSQL holds, since 2000-01-01:
/// `MIN_TIMESTAMP`.
pub(crate) const MIN_TIMESTAMP: i64 = -211_813_488_000_000_000;

/// The last microsecond PostgreSQL holds, since 2000-01-01: one before
/// `END_TIMESTAMP`.
pub(crate) const MAX_TIMESTAMP: i64 = 9_223_371_331_200_000_000 - 1;

/// A stored count of days or of a unit of time, and how it becomes
/// PostgreSQL's count: `stored * scale - shift`, valid where `stored` is
/// within `low..=high`, the bounds in the stored unit of PostgreSQL's
/// range, clamped to what an `i64` holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Count {
    pub(crate) scale: i64,
    pub(crate) shift: i64,
    pub(crate) low: i64,
    pub(crate) high: i64,
}

impl Count {
    /// Days since 1970-01-01 into days since 2000-01-01.
    pub(crate) const DAYS: Count = Count {
        scale: 1,
        shift: EPOCH_DAYS,
        low: MIN_DATE + EPOCH_DAYS,
        high: MAX_DATE + EPOCH_DAYS,
    };

    /// A unit of time of `units_per_second` a second since 1970 into
    /// microseconds since 2000.
    pub(crate) const fn time(units_per_second: i64) -> Count {
        let scale = 1_000_000 / units_per_second;
        let scale_wide = scale as i128;
        let shift = EPOCH_USECS as i128;
        // The least stored value whose microseconds reach MIN_TIMESTAMP,
        // and the greatest that stays within MAX_TIMESTAMP, by exact
        // division in 128 bits.
        let low = div_ceil(MIN_TIMESTAMP as i128 + shift, scale_wide);
        let high = div_floor(MAX_TIMESTAMP as i128 + shift, scale_wide);
        Count {
            scale,
            shift: EPOCH_USECS,
            low: clamp(low),
            high: clamp(high),
        }
    }

    /// Whether `stored` is a value of PostgreSQL's range.
    #[inline]
    pub(crate) const fn holds(self, stored: i64) -> bool {
        self.low <= stored && stored <= self.high
    }

    /// PostgreSQL's count of `stored`, exact where [`Count::holds`] is
    /// true, and of no meaning, but no overflow, elsewhere.
    #[inline]
    pub(crate) const fn convert(self, stored: i64) -> i64 {
        stored.wrapping_mul(self.scale).wrapping_sub(self.shift)
    }
}

const fn div_floor(a: i128, b: i128) -> i128 {
    let q = a / b;
    if a % b != 0 && (a < 0) != (b < 0) {
        q - 1
    } else {
        q
    }
}

const fn div_ceil(a: i128, b: i128) -> i128 {
    let q = a / b;
    if a % b != 0 && (a < 0) == (b < 0) {
        q + 1
    } else {
        q
    }
}

const fn clamp(value: i128) -> i64 {
    if value > i64::MAX as i128 {
        i64::MAX
    } else if value < i64::MIN as i128 {
        i64::MIN
    } else {
        value as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exact(count: Count, stored: i64) -> Option<i64> {
        let value = i128::from(stored) * i128::from(count.scale) - i128::from(count.shift);
        i64::try_from(value).ok()
    }

    #[test]
    fn the_epochs_and_ranges_are_postgresql_s() {
        assert_eq!(EPOCH_DAYS, 10_957);
        assert_eq!(EPOCH_USECS, 946_684_800_000_000);
        // date2j(5874897, 12, 31) - POSTGRES_EPOCH_JDATE
        assert_eq!(MAX_DATE, 2_145_031_948);
        assert_eq!(MIN_TIMESTAMP, (0 - 2_451_545) * USECS_PER_DAY);
    }

    #[test]
    fn the_bounds_of_each_unit_are_the_edges_of_the_range() {
        let days = Count::DAYS;
        for (count, min, max) in [
            (days, MIN_DATE, MAX_DATE),
            (Count::time(1), MIN_TIMESTAMP, MAX_TIMESTAMP),
            (Count::time(1_000), MIN_TIMESTAMP, MAX_TIMESTAMP),
            (Count::time(1_000_000), MIN_TIMESTAMP, MAX_TIMESTAMP),
        ] {
            assert!(count.holds(count.low) && count.holds(count.high));
            assert_eq!(Some(count.convert(count.low)), exact(count, count.low));
            assert_eq!(Some(count.convert(count.high)), exact(count, count.high));
            let low = exact(count, count.low).unwrap();
            let high = exact(count, count.high).unwrap();
            assert!(min <= low && high <= max);
            // One unit further falls out of the range, unless the bound
            // is the end of an i64.
            if count.low > i64::MIN {
                assert!(!count.holds(count.low - 1));
                assert!(exact(count, count.low - 1).is_none_or(|v| v < min));
            }
            if count.high < i64::MAX {
                assert!(!count.holds(count.high + 1));
                assert!(exact(count, count.high + 1).is_none_or(|v| v > max));
            }
        }
        // Microseconds since 1970 hold no time past PostgreSQL's last
        // thirty years, so the largest i64 is within the range.
        assert_eq!(Count::time(1_000_000).high, i64::MAX);
    }

    #[test]
    fn a_value_out_of_range_converts_without_an_overflow() {
        for count in [Count::DAYS, Count::time(1), Count::time(1_000)] {
            for stored in [i64::MIN, i64::MAX, 0, -1] {
                let _ = count.convert(stored);
            }
        }
    }
}
