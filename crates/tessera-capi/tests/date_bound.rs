//! The entry point of a date compared with a timestamp, called with raw
//! pointers as C calls it: each operator against timestamps at the edges
//! of a day, of the timestamps and the infinities, over a column of dates
//! at their edges with NULLs and unselected rows, against the core's
//! comparison written over microseconds. Miri runs it.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test reports a failure by panicking"
)]

use std::cmp::Ordering;
use std::ptr;

use tessera_capi::c::{Code, DatumColumn, Mask, Status, tess_date_timestamp_filter};

const DAY: i64 = 86_400_000_000;
const DATE_NOBEGIN: i32 = i32::MIN;
const DATE_NOEND: i32 = i32::MAX;
/// The first date past the timestamps, 294277-01-01.
const PAST: i32 = 106_751_983;

/// `TessCompareOp`: = <> < <= > >=.
const OPS: [u32; 6] = [0, 1, 2, 3, 4, 5];

/// The core's `date_cmp_timestamp`, over microseconds in i128.
fn compare(date: i32, timestamp: i64) -> Ordering {
    match (date, timestamp) {
        (DATE_NOBEGIN, i64::MIN) | (DATE_NOEND, i64::MAX) => Ordering::Equal,
        (DATE_NOBEGIN, _) | (_, i64::MAX) => Ordering::Less,
        (DATE_NOEND, _) | (_, i64::MIN) => Ordering::Greater,
        _ if date >= PAST => Ordering::Greater,
        _ => (i128::from(date) * i128::from(DAY)).cmp(&i128::from(timestamp)),
    }
}

fn holds(op: u32, ordering: Ordering) -> bool {
    match op {
        0 => ordering == Ordering::Equal,
        1 => ordering != Ordering::Equal,
        2 => ordering == Ordering::Less,
        3 => ordering != Ordering::Greater,
        4 => ordering == Ordering::Greater,
        _ => ordering != Ordering::Less,
    }
}

#[test]
fn each_operator_keeps_the_rows_of_the_core() {
    let dates = [
        DATE_NOBEGIN,
        -2_451_545,
        -1,
        0,
        1,
        2,
        PAST - 1,
        PAST,
        2_145_031_948,
        DATE_NOEND,
    ];
    let nrows = 70;
    let values: Vec<i32> = (0..nrows).map(|row| dates[row % dates.len()]).collect();
    let datums: Vec<u64> = values.iter().map(|&d| i64::from(d) as u64).collect();
    let isnull: Vec<bool> = (0..nrows).map(|row| row % 13 == 6).collect();
    let column = DatumColumn {
        struct_size: size_of::<DatumColumn>(),
        values: datums.as_ptr(),
        isnull: isnull.as_ptr(),
        nrows: nrows as i32,
        ..DatumColumn::EMPTY
    };
    let selected = [!(1_u64 << 11), 0b0010_1111];
    let mut stamps = vec![
        i64::MIN,
        i64::MAX,
        -211_813_488_000_000_000,
        9_223_371_331_199_999_999,
    ];
    for day in [-1_i64, 0, 1] {
        for time in [-1, 0, 1, DAY / 2] {
            stamps.push(day * DAY + time);
        }
    }
    for op in OPS {
        for &timestamp in &stamps {
            let mut words = selected;
            let mut rows = Mask {
                nrows: nrows as i32,
                bits: words.as_mut_ptr(),
            };
            let mut status = Status::new();
            // SAFETY: a local column, mask and status, alive for the call.
            let code = unsafe {
                tess_date_timestamp_filter(
                    &column,
                    ptr::null(),
                    &mut rows,
                    op,
                    timestamp,
                    &mut status,
                )
            };
            assert_eq!(code, Code::Ok, "{}", status.message());
            for row in 0..nrows {
                let chosen = selected[row / 64] >> (row % 64) & 1 == 1;
                let want = chosen && !isnull[row] && holds(op, compare(values[row], timestamp));
                let kept = words[row / 64] >> (row % 64) & 1 == 1;
                assert_eq!(
                    kept, want,
                    "op {op} timestamp {timestamp} row {row} date {}",
                    values[row]
                );
            }
        }
    }
}

#[test]
fn an_unknown_operator_is_refused() {
    let datums = [0_u64];
    let isnull = [false];
    let column = DatumColumn {
        struct_size: size_of::<DatumColumn>(),
        values: datums.as_ptr(),
        isnull: isnull.as_ptr(),
        nrows: 1,
        ..DatumColumn::EMPTY
    };
    let mut words = [1_u64];
    let mut rows = Mask {
        nrows: 1,
        bits: words.as_mut_ptr(),
    };
    let mut status = Status::new();
    // SAFETY: as above.
    let code =
        unsafe { tess_date_timestamp_filter(&column, ptr::null(), &mut rows, 9, 0, &mut status) };
    assert_eq!(code, Code::InvalidArgument);
}
