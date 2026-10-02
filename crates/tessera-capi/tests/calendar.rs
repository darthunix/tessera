//! The calendar entry points called with raw pointers, as C calls them:
//! columns and scalars, the errors as statuses, the masks of the rows left
//! to the caller.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test reports a failure by panicking"
)]

use std::ptr;

use tessera_capi::c::{
    CalendarArg, Code, DatumColumn, Mask, Status, tess_date_add_interval, tess_date_arith,
    tess_date_extract, tess_date_to_timestamp, tess_timestamp_add_interval, tess_timestamp_extract,
    tess_timestamp_to_date, tess_timestamp_trunc, tess_timestamp_trunc_local,
};
use tessera_kernels::calendar::{
    DATE_NOEND, Interval, POSTGRES_EPOCH_JDATE, TIMESTAMP_NOEND, USECS_PER_DAY, USECS_PER_HOUR,
    date_to_julian,
};

fn column(values: &[u64], isnull: &[bool]) -> DatumColumn {
    DatumColumn {
        struct_size: size_of::<DatumColumn>(),
        values: values.as_ptr(),
        isnull: isnull.as_ptr(),
        nrows: values.len() as i32,
        ..DatumColumn::EMPTY
    }
}

fn mask(nrows: usize, words: &mut [u64]) -> Mask {
    Mask {
        nrows: nrows as i32,
        bits: words.as_mut_ptr(),
    }
}

fn arg(column: &DatumColumn) -> CalendarArg {
    CalendarArg { column, scalar: 0 }
}

fn scalar(value: u64) -> CalendarArg {
    CalendarArg {
        column: ptr::null(),
        scalar: value,
    }
}

/// Days since 2000-01-01 of a date.
fn day(year: i32, month: i32, mday: i32) -> i32 {
    date_to_julian(year, month, mday) - POSTGRES_EPOCH_JDATE
}

#[test]
fn dates_add_and_fail_as_the_core() {
    let dates = [day(2024, 2, 28) as u32 as u64, 0, DATE_NOEND as u32 as u64];
    let isnull = [false, true, false];
    let left = column(&dates, &isnull);
    let rows = [0b111_u64];
    let mut values = [0_i32; 3];
    let mut present = [0_u64];
    let mut status = Status::new();
    // SAFETY: local columns, masks, buffers and status.
    let code = unsafe {
        tess_date_arith(
            0,
            &arg(&left),
            &scalar(2),
            &mask(3, &mut rows.clone()),
            values.as_mut_ptr(),
            &mut mask(3, &mut present),
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok, "{}", status.message());
    assert_eq!(present[0], 0b101);
    assert_eq!((values[0], values[2]), (day(2024, 3, 1), DATE_NOEND));
    // date - date with an infinite date: 22008.
    // SAFETY: as above.
    let code = unsafe {
        tess_date_arith(
            2,
            &arg(&left),
            &scalar(0),
            &mask(3, &mut rows.clone()),
            values.as_mut_ptr(),
            &mut mask(3, &mut present),
            &raw mut status,
        )
    };
    assert_eq!(code, Code::DataException);
    assert_eq!(
        (status.sqlstate(), status.message()),
        (
            "22008".to_owned(),
            "cannot subtract infinite dates".to_owned()
        )
    );
    // The casts.
    let mut stamps = [0_i64; 3];
    // SAFETY: as above.
    let code = unsafe {
        tess_date_to_timestamp(
            &arg(&left),
            &mask(3, &mut rows.clone()),
            stamps.as_mut_ptr(),
            &mut mask(3, &mut present),
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok);
    assert_eq!(
        (stamps[0], stamps[2]),
        (i64::from(day(2024, 2, 28)) * USECS_PER_DAY, TIMESTAMP_NOEND)
    );
    let back_values: Vec<u64> = stamps.iter().map(|&stamp| stamp as u64).collect();
    let back = column(&back_values, &isnull);
    let mut dates_back = [0_i32; 3];
    // SAFETY: as above.
    let code = unsafe {
        tess_timestamp_to_date(
            &arg(&back),
            &mask(3, &mut rows.clone()),
            dates_back.as_mut_ptr(),
            &mut mask(3, &mut present),
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok);
    assert_eq!(
        (dates_back[0], dates_back[2]),
        (day(2024, 2, 28), DATE_NOEND)
    );
}

#[test]
fn truncation_of_timestamps_and_local_times() {
    let t = i64::from(day(2023, 8, 17)) * USECS_PER_DAY + 13 * USECS_PER_HOUR;
    let stamps = [t as u64, TIMESTAMP_NOEND as u64];
    let isnull = [false, false];
    let source = column(&stamps, &isnull);
    let rows = [0b11_u64];
    let mut values = [0_i64; 2];
    let mut present = [0_u64];
    let mut status = Status::new();
    // SAFETY: local columns, masks, buffers and status.
    let code = unsafe {
        tess_timestamp_trunc(
            7,
            &arg(&source),
            &mask(2, &mut rows.clone()),
            values.as_mut_ptr(),
            &mut mask(2, &mut present),
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok, "{}", status.message());
    assert_eq!(
        values,
        [i64::from(day(2023, 8, 1)) * USECS_PER_DAY, TIMESTAMP_NOEND]
    );
    // Local times: a month's first day as a Julian day, an hour as a time,
    // an infinity to the rest.
    let (mut days, mut rest) = ([0_u64], [0_u64]);
    // SAFETY: as above.
    let code = unsafe {
        tess_timestamp_trunc_local(
            7,
            &source,
            &mask(2, &mut rows.clone()),
            values.as_mut_ptr(),
            &mut mask(2, &mut days),
            &mut mask(2, &mut rest),
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok);
    assert_eq!((days[0], rest[0]), (0b01, 0b10));
    assert_eq!(values[0], i64::from(date_to_julian(2023, 8, 1)));
    // SAFETY: as above.
    let code = unsafe {
        tess_timestamp_trunc_local(
            4,
            &source,
            &mask(2, &mut rows.clone()),
            values.as_mut_ptr(),
            &mut mask(2, &mut days),
            &mut mask(2, &mut rest),
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok);
    assert_eq!((days[0], rest[0], values[0]), (0, 0b10, t));
    // An unknown unit fails as an invalid argument.
    // SAFETY: as above.
    let code = unsafe {
        tess_timestamp_trunc(
            13,
            &arg(&source),
            &mask(2, &mut rows.clone()),
            values.as_mut_ptr(),
            &mut mask(2, &mut present),
            &raw mut status,
        )
    };
    assert_eq!(code, Code::InvalidArgument);
}

#[test]
fn intervals_from_columns_of_pointers_and_scalars() {
    let t = i64::from(day(2024, 1, 31)) * USECS_PER_DAY;
    let spans = [
        Interval {
            time: 0,
            day: 0,
            month: 1,
        },
        Interval {
            time: 0,
            day: -1,
            month: 0,
        },
    ];
    let span_values: Vec<u64> = spans
        .iter()
        .map(|span| ptr::from_ref(span) as u64)
        .collect();
    let isnull = [false, false];
    let span_column = column(&span_values, &isnull);
    let rows = [0b11_u64];
    let mut values = [0_i64; 2];
    let mut present = [0_u64];
    let mut status = Status::new();
    // SAFETY: local columns, masks, buffers and status; the scalar and the
    // column's Datums point to live intervals.
    let code = unsafe {
        tess_timestamp_add_interval(
            false,
            &scalar(t as u64),
            &arg(&span_column),
            &mask(2, &mut rows.clone()),
            values.as_mut_ptr(),
            &mut mask(2, &mut present),
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok, "{}", status.message());
    assert_eq!(
        values,
        [
            i64::from(day(2024, 2, 29)) * USECS_PER_DAY,
            i64::from(day(2024, 1, 30)) * USECS_PER_DAY
        ]
    );
    // A date past the timestamps fails before its interval.
    let dates = [i32::MAX as u32 as u64 - 1, day(2000, 1, 1) as u32 as u64];
    let date_column = column(&dates, &isnull);
    // SAFETY: as above.
    let code = unsafe {
        tess_date_add_interval(
            true,
            &arg(&date_column),
            &scalar(ptr::from_ref(&spans[0]) as u64),
            &mask(2, &mut rows.clone()),
            values.as_mut_ptr(),
            &mut mask(2, &mut present),
            &raw mut status,
        )
    };
    assert_eq!(code, Code::DataException);
    assert_eq!(status.message(), "date out of range for timestamp");
}

#[test]
fn fields_with_their_scales_and_the_rest() {
    let t = i64::from(day(2021, 1, 3)) * USECS_PER_DAY + 13 * USECS_PER_HOUR + 5_250_000;
    let stamps = [t as u64, TIMESTAMP_NOEND as u64, 0];
    let isnull = [false, false, true];
    let source = column(&stamps, &isnull);
    let rows = [0b111_u64];
    let (mut values, mut scales) = ([0_i64; 3], [0_u8; 3]);
    let (mut present, mut rest) = ([0_u64], [0_u64]);
    let mut status = Status::new();
    // SAFETY: local columns, masks, buffers and status.
    let code = unsafe {
        tess_timestamp_extract(
            2,
            &arg(&source),
            &mask(3, &mut rows.clone()),
            values.as_mut_ptr(),
            scales.as_mut_ptr(),
            &mut mask(3, &mut present),
            &mut mask(3, &mut rest),
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok, "{}", status.message());
    assert_eq!((present[0], rest[0]), (0b011, 0b010));
    assert_eq!((values[0], scales[0]), (5_250_000, 6));
    let dates = [day(2021, 1, 3) as u32 as u64];
    let date_column = column(&dates, &[false]);
    // Result masks of one row come without bits past it.
    (present, rest) = ([0], [0]);
    // SAFETY: as above.
    let code = unsafe {
        tess_date_extract(
            8,
            &arg(&date_column),
            &mask(1, &mut [1]),
            values.as_mut_ptr(),
            scales.as_mut_ptr(),
            &mut mask(1, &mut present),
            &mut mask(1, &mut rest),
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok);
    assert_eq!((values[0], scales[0], rest[0]), (53, 0, 0));
}
