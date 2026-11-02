//! The Kani proof of the bound of a date against a timestamp: for every
//! valid date with the infinities, every day and time of a valid finite
//! timestamp and the two infinite timestamps, and each of the six
//! operators, the comparison of the date with the bound is the core's
//! comparison. A finite timestamp enters as its day and time, so that the
//! proof needs no division; `split`, which makes them, is checked by the
//! tests.

use super::reference::{compare_split, holds};
use super::{
    DATE_NOBEGIN, DATE_NOEND, POSTGRES_EPOCH_JDATE, TIMESTAMP_NOBEGIN, TIMESTAMP_NOEND,
    USECS_PER_DAY, date_bound, day_bound,
};
use crate::ops::CompareOp;

/// The last finite date, 5874897-12-31.
const LAST_DATE: i32 = 2_147_483_493 - POSTGRES_EPOCH_JDATE;

/// The day after the last finite timestamp's, 294277-01-01.
const PAST_TIMESTAMPS: i64 = 106_751_983;

#[kani::proof]
fn the_bound_gives_the_cores_answer() {
    let op = match kani::any::<u8>() % 6 {
        0 => CompareOp::Eq,
        1 => CompareOp::Ne,
        2 => CompareOp::Lt,
        3 => CompareOp::Le,
        4 => CompareOp::Gt,
        _ => CompareOp::Ge,
    };
    let date: i32 = kani::any();
    kani::assume(
        date == DATE_NOBEGIN
            || date == DATE_NOEND
            || (-POSTGRES_EPOCH_JDATE..=LAST_DATE).contains(&date),
    );
    let day: i64 = kani::any();
    let time: i64 = kani::any();
    kani::assume((-i64::from(POSTGRES_EPOCH_JDATE)..PAST_TIMESTAMPS).contains(&day));
    kani::assume((0..USECS_PER_DAY).contains(&time));
    let (timestamp, (bound_op, bound)) = match kani::any::<u8>() % 3 {
        0 => (TIMESTAMP_NOBEGIN, date_bound(op, TIMESTAMP_NOBEGIN)),
        1 => (TIMESTAMP_NOEND, date_bound(op, TIMESTAMP_NOEND)),
        // A finite timestamp, which only its day and time stand for.
        _ => (0, day_bound(op, day, time)),
    };
    let want = holds(op, compare_split(date, timestamp, day, time));
    assert!(holds(bound_op, date.cmp(&bound)) == want);
    // Each operator meets both outcomes: one cover a place.
    macro_rules! covers {
        ($($each:ident),*) => {
            $(
                kani::cover!(op == CompareOp::$each && want);
                kani::cover!(op == CompareOp::$each && !want);
            )*
        };
    }
    covers!(Eq, Ne, Lt, Le, Gt, Ge);
}
