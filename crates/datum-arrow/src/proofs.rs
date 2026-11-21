//! The proofs by Kani of the code of one value, for every value rather
//! than for samples: `cargo kani -p datum-arrow` runs them. Kani also
//! checks every arithmetic overflow, index and panic on the way, so a
//! proof shows too that the code cannot panic. The loops over a batch
//! are compared with this code by the property tests.

use crate::bounds::{Comparison, RangeAnswer, Test};
use crate::calendar::{
    Count, EPOCH_DAYS, EPOCH_USECS, MAX_DATE, MAX_TIMESTAMP, MIN_DATE, MIN_TIMESTAMP,
};
use crate::pg::PgType;
use crate::read::{ReadOptions, ReadPlan, Unit};
use crate::write::{date_of, time_of};

const SECONDS: Count = Count::time(1);
const MILLISECONDS: Count = Count::time(1_000);
const MICROSECONDS: Count = Count::time(1_000_000);

fn any_comparison() -> Comparison {
    match kani::any::<u8>() % 6 {
        0 => Comparison::Eq,
        1 => Comparison::Ne,
        2 => Comparison::Lt,
        3 => Comparison::Le,
        4 => Comparison::Gt,
        _ => Comparison::Ge,
    }
}

/// `value op constant`, in 128 bits.
fn compare(op: Comparison, value: i128, constant: i128) -> bool {
    match op {
        Comparison::Eq => value == constant,
        Comparison::Ne => value != constant,
        Comparison::Lt => value < constant,
        Comparison::Le => value <= constant,
        Comparison::Gt => value > constant,
        Comparison::Ge => value >= constant,
    }
}

/// A count read: accepted exactly when its exact value, in 128 bits, is
/// in PostgreSQL's range, and then converted to that value.
fn check_count(count: Count, min: i64, max: i64) {
    let stored: i64 = kani::any();
    let exact = i128::from(stored) * i128::from(count.scale) - i128::from(count.shift);
    let in_range = i128::from(min) <= exact && exact <= i128::from(max);
    assert!(count.holds(stored) == in_range);
    if in_range {
        assert!(i128::from(count.convert(stored)) == exact);
    }
    kani::cover!(in_range, "a value in range");
    kani::cover!(!in_range, "a value out of range");
}

#[kani::proof]
fn days_read_exactly_in_postgresql_s_range() {
    check_count(Count::DAYS, MIN_DATE, MAX_DATE);
}

#[kani::proof]
fn seconds_read_exactly_in_postgresql_s_range() {
    check_count(SECONDS, MIN_TIMESTAMP, MAX_TIMESTAMP);
}

#[kani::proof]
fn milliseconds_read_exactly_in_postgresql_s_range() {
    check_count(MILLISECONDS, MIN_TIMESTAMP, MAX_TIMESTAMP);
}

#[kani::proof]
fn microseconds_read_exactly_in_postgresql_s_range() {
    check_count(MICROSECONDS, MIN_TIMESTAMP, MAX_TIMESTAMP);
}

/// A date written: accepted exactly when it is in PostgreSQL's range, an
/// infinity not, and read back to the same days.
#[kani::proof]
fn dates_written_in_range_and_read_back() {
    let days: i32 = kani::any();
    let in_range = (MIN_DATE..=MAX_DATE).contains(&i64::from(days));
    match date_of(days) {
        Ok(written) => {
            assert!(in_range);
            assert!(i64::from(written) == i64::from(days) + EPOCH_DAYS);
            assert!(Count::DAYS.holds(i64::from(written)));
            assert!(Count::DAYS.convert(i64::from(written)) == i64::from(days));
        }
        Err(_) => assert!(!in_range),
    }
    kani::cover!(days == i32::MAX, "infinity");
}

/// A time written: accepted exactly when it is in PostgreSQL's range and
/// its microseconds since 1970 fit an i64, an infinity not, and read back
/// to the same microseconds.
#[kani::proof]
fn times_written_in_range_and_read_back() {
    let micros: i64 = kani::any();
    let fits = i128::from(micros) + i128::from(EPOCH_USECS) <= i128::from(i64::MAX);
    let in_range = (MIN_TIMESTAMP..=MAX_TIMESTAMP).contains(&micros) && fits;
    match time_of(micros) {
        Ok(written) => {
            assert!(in_range);
            assert!(MICROSECONDS.holds(written));
            assert!(MICROSECONDS.convert(written) == micros);
        }
        Err(_) => assert!(!in_range),
    }
    kani::cover!(
        !fits && micros <= MAX_TIMESTAMP,
        "a time of the last thirty years"
    );
}

/// The test of a bound passes a stored value exactly when the comparison
/// of its exact count with the constant is true, for every stored value
/// and every finite constant.
fn check_bound(data_type: &arrow_schema::DataType, pg: PgType, options: ReadOptions, count: Count) {
    let Ok(plan) = ReadPlan::new(data_type, pg, options) else {
        panic!("the plans are made");
    };
    let op = any_comparison();
    let constant: i64 = kani::any();
    let infinite = match pg {
        PgType::Date => constant == i64::from(i32::MAX) || constant == i64::from(i32::MIN),
        PgType::Timestamp | PgType::Timestamptz => constant == i64::MAX || constant == i64::MIN,
        _ => false,
    };
    kani::assume(!infinite);
    let Ok(test) = plan.test(op, Some(constant)) else {
        panic!("a plan of integers or times takes bounds");
    };
    let stored: i64 = kani::any();
    let exact = i128::from(stored) * i128::from(count.scale) - i128::from(count.shift);
    assert!(test.passes(stored) == compare(op, exact, i128::from(constant)));
    kani::cover!(matches!(test, Test::Compare { .. }), "a bound");
}

#[kani::proof]
fn bounds_of_integers_are_exact() {
    let plain = ReadOptions {
        unit: None,
        text: None,
        server_utf8: true,
    };
    let count = Count {
        scale: 1,
        shift: 0,
        low: i64::MIN,
        high: i64::MAX,
    };
    check_bound(&arrow_schema::DataType::Int64, PgType::Int8, plain, count);
}

#[kani::proof]
fn bounds_of_days_are_exact() {
    let options = ReadOptions {
        unit: Some(Unit::Days),
        text: None,
        server_utf8: true,
    };
    check_bound(
        &arrow_schema::DataType::Int32,
        PgType::Date,
        options,
        Count::DAYS,
    );
}

// The bounds of a unit of a second or a millisecond divide the constant
// by a million or a thousand, which Kani does not prove within the limit
// of a harness; the property tests compare them with the exact count
// over the edges instead.

#[kani::proof]
fn bounds_of_microseconds_are_exact() {
    let options = ReadOptions {
        unit: Some(Unit::Microseconds),
        text: None,
        server_utf8: true,
    };
    check_bound(
        &arrow_schema::DataType::Int64,
        PgType::Timestamptz,
        options,
        MICROSECONDS,
    );
}

/// The answer for a range is exact: a range turned the wrong way gets
/// "some may"; on every value, each value of the range passes; on no
/// value, none does; and on "some may", the range holds a value that
/// passes and one that does not, found at its ends or at the bound.
#[kani::proof]
fn answers_for_a_range_are_exact() {
    let test = match kani::any::<u8>() % 3 {
        0 => Test::Every,
        1 => Test::Nothing,
        _ => Test::Compare {
            op: any_comparison(),
            bound: kani::any(),
        },
    };
    let least: i64 = kani::any();
    let greatest: i64 = kani::any();
    let answer = test.range(least, greatest);
    if least > greatest {
        assert!(answer == RangeAnswer::SomeMay);
        return;
    }
    let value: i64 = kani::any();
    kani::assume(least <= value && value <= greatest);
    match answer {
        RangeAnswer::EveryValue => assert!(test.passes(value)),
        RangeAnswer::NoValue => assert!(!test.passes(value)),
        RangeAnswer::SomeMay => match test {
            Test::Compare {
                op: Comparison::Eq | Comparison::Ne,
                bound,
            } => {
                // The bound is in the range, and an end of it is not.
                assert!(least <= bound && bound <= greatest && least < greatest);
                let other = if least == bound { greatest } else { least };
                assert!(test.passes(bound) != test.passes(other));
            }
            Test::Compare { .. } => assert!(test.passes(least) != test.passes(greatest)),
            Test::Every | Test::Nothing => panic!("every value or none"),
        },
    }
    kani::cover!(answer == RangeAnswer::SomeMay, "some may");
    kani::cover!(answer == RangeAnswer::EveryValue, "every value");
    kani::cover!(answer == RangeAnswer::NoValue, "no value");
}

/// A string of up to four bytes read through a plan of checked bytes
/// into `text`: refused exactly where it is not UTF-8 or holds a zero
/// byte, with `22021` and its slot, and given as its bytes otherwise.
/// The check of UTF-8 is the standard library's on both sides: the proof
/// shows the plan's checks wired, not UTF-8 itself.
#[kani::proof]
#[kani::unwind(6)]
fn checked_text_is_refused_exactly() {
    use crate::column::{Column, Input, Values};
    use crate::read::{ReadOutput, TextBytes};
    let bytes: [u8; 4] = kani::any();
    let len: u8 = kani::any();
    kani::assume(len <= 4);
    let offsets = [0i32, i32::from(len)];
    let options = ReadOptions {
        unit: None,
        text: Some(TextBytes::Checked),
        server_utf8: true,
    };
    let Ok(plan) = ReadPlan::new(&arrow_schema::DataType::Binary, PgType::Text, options) else {
        panic!("the plan is made");
    };
    let input = Input::Plain(Column {
        len: 1,
        values: Values::Bytes {
            offsets: &offsets,
            data: &bytes,
        },
        nulls: None,
    });
    let mut out: [&[u8]; 1] = [&[]];
    let mut nulls = [0u64; 1];
    let output = &mut ReadOutput::Bytes {
        values: &mut out,
        nulls: &mut nulls,
    };
    let string = &bytes[..usize::from(len)];
    let mut zero = false;
    for &byte in string {
        zero |= byte == 0;
    }
    let valid = core::str::from_utf8(string).is_ok() && !zero;
    match plan.read(&input, &[1], output) {
        Ok(()) => {
            assert!(valid);
            assert!(out[0] == string);
        }
        Err(error) => {
            assert!(!valid);
            assert!(error.sqlstate() == "22021" && error.slot() == Some(0));
        }
    }
    kani::cover!(
        zero && core::str::from_utf8(string).is_ok(),
        "UTF-8 with a zero byte"
    );
}

/// Reads a column of one slot of `values` through the plan of
/// `data_type` into `pg`: a plan that names no failure reads every value
/// without one, and a plan of times fails exactly where the value is out
/// of range.
fn check_failures(
    data_type: &arrow_schema::DataType,
    pg: PgType,
    unit: Option<Unit>,
    values: crate::column::Values<'_>,
    out_of_range: bool,
) {
    use crate::column::{Column, Input};
    use crate::error::ErrorKind;
    use crate::failures::Failures;
    use crate::read::ReadOutput;
    let options = ReadOptions {
        unit,
        text: None,
        server_utf8: true,
    };
    let Ok(plan) = ReadPlan::new(data_type, pg, options) else {
        panic!("the plans are made");
    };
    let input = Input::Plain(Column {
        len: 1,
        values,
        nulls: None,
    });
    let mut out = [0u64; 1];
    let mut nulls = [0u64; 1];
    let output = &mut ReadOutput::Datums {
        values: &mut out,
        nulls: &mut nulls,
    };
    let names_range = plan.failures().contains(Failures::OUT_OF_RANGE);
    assert!(plan.failures().is_empty() || plan.failures() == Failures::OUT_OF_RANGE);
    let result = plan.read(&input, &[1], output);
    // The read fails exactly where the value is out of range, and only
    // where the plan names that failure.
    assert!(result.is_err() == out_of_range);
    assert!(!out_of_range || names_range);
    if let Err(error) = result {
        assert!(error.kind() == ErrorKind::OutOfRange && error.slot() == Some(0));
    }
    kani::cover!(result.is_ok(), "a value read");
    kani::cover!(
        result.is_err() || !names_range,
        "a value out of range, where it can be"
    );
}

/// Rule "A plan names every failure" of the contract: integers widened
/// never fail.
#[kani::proof]
fn integers_never_fail() {
    let values: [i16; 1] = [kani::any()];
    let data_type = arrow_schema::DataType::Int16;
    check_failures(
        &data_type,
        PgType::Int2,
        None,
        crate::column::Values::Int16(&values),
        false,
    );
}

/// Days in 16 bits, which every `date` holds, never fail.
#[kani::proof]
fn short_days_never_fail() {
    let values: [u16; 1] = [kani::any()];
    let data_type = arrow_schema::DataType::UInt16;
    check_failures(
        &data_type,
        PgType::Date,
        Some(Unit::Days),
        crate::column::Values::UInt16(&values),
        false,
    );
}

/// 64-bit integers into `bigint` never fail.
#[kani::proof]
fn long_integers_never_fail() {
    let values: [i64; 1] = [kani::any()];
    let data_type = arrow_schema::DataType::Int64;
    check_failures(
        &data_type,
        PgType::Int8,
        None,
        crate::column::Values::Int64(&values),
        false,
    );
}

/// Seconds fail exactly where the value is out of range.
#[kani::proof]
fn seconds_fail_only_out_of_range() {
    let values: [i64; 1] = [kani::any()];
    let data_type = arrow_schema::DataType::Int64;
    check_failures(
        &data_type,
        PgType::Timestamp,
        Some(Unit::Seconds),
        crate::column::Values::Int64(&values),
        !(i128::from(MIN_TIMESTAMP)..=i128::from(MAX_TIMESTAMP))
            .contains(&(i128::from(values[0]) * 1_000_000 - i128::from(EPOCH_USECS))),
    );
}

/// The count of a `varchar`'s characters is the count of the characters
/// of a valid UTF-8 string, for strings of up to four bytes, and the
/// check of a zero byte finds one where a plain loop does.
#[kani::proof]
#[kani::unwind(6)]
fn characters_and_zero_bytes_of_short_strings() {
    let bytes: [u8; 4] = kani::any();
    let len: usize = kani::any();
    kani::assume(len <= 4);
    let bytes = &bytes[..len];
    let mut zero = false;
    for &byte in bytes {
        zero |= byte == 0;
    }
    assert!(crate::read::has_zero(bytes) == zero);
    if let Ok(text) = core::str::from_utf8(bytes) {
        // The characters decoded one by one, not counted by their bytes.
        let mut decoded = 0;
        for _ in text.chars() {
            decoded += 1;
        }
        assert!(crate::read::count_chars(bytes) == decoded);
        kani::cover!(decoded < len, "a character of several bytes");
        kani::cover!(len == 4 && decoded == 1, "a character of four bytes");
    }
}

/// Every count's shift is a whole number of its stored unit, which the
/// bounds of a comparison count on to divide the constant in 64 bits.
#[kani::proof]
fn every_shift_is_whole_units() {
    for count in [Count::DAYS, SECONDS, MILLISECONDS, MICROSECONDS] {
        assert!(count.scale > 0 && count.shift % count.scale == 0);
    }
}
