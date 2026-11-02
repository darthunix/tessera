//! The entry points of the local time of a timestamptz, called with raw
//! pointers as C calls them: rounds of questions and answers until no row
//! is unknown, over a column and a scalar, against a zone of a few
//! transitions; the reset; and the calls a caller gets wrong. Miri runs it.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test reports a failure by panicking"
)]

use std::mem::MaybeUninit;
use std::ptr;

use tessera_capi::c::{
    CalendarArg, Code, DatumColumn, Mask, Status, ZONE_MIDNIGHT, ZONE_SPAN, ZoneAnswer, ZoneCache,
    ZoneQuestion, ZoneRound, tess_zone_dates, tess_zone_locals, tess_zone_midnights,
    tess_zone_reset, tess_zone_truncate,
};

const USECS: i64 = 1_000_000;
const DAY: i64 = 86_400;
/// The seconds from 1970-01-01 to 2000-01-01.
const EPOCH: i64 = 946_684_800;
/// 2000-01-01 as a Julian day.
const JULIAN_2000: i64 = 2_451_545;
/// `TESS_UNIT_HOUR` and `TESS_UNIT_DAY`.
const HOUR_UNIT: i32 = 4;
const DAY_UNIT: i32 = 5;

/// A zone one hour east of UTC, two hours from the second 2000-04-01 on,
/// one hour again from 2000-10-01 on.
fn offset(second: i64) -> i64 {
    let (spring, autumn) = (EPOCH + 91 * DAY, EPOCH + 274 * DAY);
    if (spring..autumn).contains(&second) {
        7200
    } else {
        3600
    }
}

fn next(second: i64) -> Option<i64> {
    [EPOCH + 91 * DAY, EPOCH + 274 * DAY]
        .into_iter()
        .find(|&at| at > second)
}

/// The library's answer to a question.
fn answer(question: &ZoneQuestion) -> ZoneAnswer {
    match question.kind {
        ZONE_SPAN => ZoneAnswer {
            key: question.key,
            offset: offset(question.key),
            boundary: next(question.key).unwrap_or(0),
            kind: ZONE_SPAN,
            found: i32::from(next(question.key).is_some()),
        },
        _ => {
            let midnight = (question.key - JULIAN_2000) * DAY + EPOCH;
            ZoneAnswer {
                key: question.key,
                offset: -offset(midnight - offset(midnight)),
                boundary: 0,
                kind: ZONE_MIDNIGHT,
                found: 0,
            }
        }
    }
}

/// One operation of the module.
#[derive(Clone, Copy, Debug)]
enum Op {
    Locals,
    Truncate(i32),
    Dates,
    Midnights,
}

/// Run an operation in rounds over `arg`'s `nrows` rows, as C runs it:
/// every answer so far given to each call, the unknown rows the next
/// round's. The values of the ready rows, `None` for the core's, and the
/// questions asked.
fn run_asking(
    cache: &mut ZoneCache,
    op: Op,
    arg: &CalendarArg,
    nrows: usize,
) -> (Vec<Option<i64>>, usize) {
    let words = nrows.div_ceil(64);
    let mut rows_words = vec![u64::MAX >> (64 * words - nrows); words];
    for word in rows_words.iter_mut().take(words - 1) {
        *word = u64::MAX;
    }
    let (mut ready_words, mut core_words) = (vec![0; words], vec![0; words]);
    let mut answers: Vec<ZoneAnswer> = Vec::new();
    let mut values64 = vec![MaybeUninit::<i64>::uninit(); nrows];
    let mut values32 = vec![MaybeUninit::<i32>::uninit(); nrows];
    let mut asked_all = 0;
    for round in 0.. {
        assert!(round < 2 * nrows + 2, "the rounds do not end");
        let mut unknown_words = vec![0; words];
        let rows = Mask {
            nrows: nrows as i32,
            bits: rows_words.as_mut_ptr(),
        };
        let mut ready = Mask {
            nrows: nrows as i32,
            bits: ready_words.as_mut_ptr(),
        };
        let mut core = Mask {
            nrows: nrows as i32,
            bits: core_words.as_mut_ptr(),
        };
        let mut unknown = Mask {
            nrows: nrows as i32,
            bits: unknown_words.as_mut_ptr(),
        };
        let mut questions = vec![MaybeUninit::<ZoneQuestion>::uninit(); nrows];
        let mut call = ZoneRound {
            struct_size: size_of::<ZoneRound>(),
            cache,
            answers: answers.as_ptr(),
            nanswers: answers.len() as i32,
            rows: &rows,
            ready: &mut ready,
            core: &mut core,
            unknown: &mut unknown,
            questions: questions.as_mut_ptr().cast(),
            nquestions: -1,
        };
        let mut status = Status::new();
        // SAFETY: a local round, argument, buffers and status.
        let code = unsafe {
            match op {
                Op::Locals => {
                    tess_zone_locals(&mut call, arg, values64.as_mut_ptr().cast(), &mut status)
                }
                Op::Truncate(unit) => tess_zone_truncate(
                    &mut call,
                    unit,
                    arg,
                    values64.as_mut_ptr().cast(),
                    &mut status,
                ),
                Op::Dates => {
                    tess_zone_dates(&mut call, arg, values32.as_mut_ptr().cast(), &mut status)
                }
                Op::Midnights => {
                    tess_zone_midnights(&mut call, arg, values64.as_mut_ptr().cast(), &mut status)
                }
            }
        };
        assert_eq!(code, Code::Ok, "{op:?}: {}", status.message());
        let asked = call.nquestions as usize;
        if asked == 0 {
            break;
        }
        // SAFETY: the call wrote its questions.
        let asked: Vec<ZoneQuestion> = questions[..asked]
            .iter()
            .map(|q| unsafe { q.assume_init() })
            .collect();
        asked_all += asked.len();
        answers.extend(asked.iter().map(answer));
        rows_words = unknown_words;
    }
    let values = (0..nrows)
        .map(|row| {
            let (ready, core) = (
                ready_words[row / 64] >> (row % 64) & 1 == 1,
                core_words[row / 64] >> (row % 64) & 1 == 1,
            );
            assert!(!(ready && core), "{op:?} row {row}: two outcomes");
            // SAFETY: a ready row's value was written.
            ready.then(|| unsafe {
                match op {
                    Op::Dates => i64::from(values32[row].assume_init()),
                    _ => values64[row].assume_init(),
                }
            })
        })
        .collect();
    (values, asked_all)
}

/// [`run_asking`]'s values.
fn run(cache: &mut ZoneCache, op: Op, arg: &CalendarArg, nrows: usize) -> Vec<Option<i64>> {
    run_asking(cache, op, arg, nrows).0
}

fn cache() -> Box<ZoneCache> {
    Box::new(ZoneCache {
        words: [0; 3 + 32 * 3 + 1024 * 3],
    })
}

/// Instants across both transitions and the infinities, a column: the
/// local times, date_trunc by hour and day, the dates; their days back to
/// local midnights.
#[test]
fn every_operation_over_a_column_in_rounds() {
    let seconds: Vec<i64> = (0..70)
        .map(|n| EPOCH + (n * 5 % 365) * DAY + n * 3600 + 59)
        .collect();
    let mut datums: Vec<u64> = seconds
        .iter()
        .map(|s| ((s - EPOCH) * USECS) as u64)
        .collect();
    datums[7] = i64::MIN as u64;
    datums[8] = i64::MAX as u64;
    let isnull: Vec<bool> = (0..70).map(|row| row == 9).collect();
    let column = DatumColumn {
        struct_size: size_of::<DatumColumn>(),
        values: datums.as_ptr(),
        isnull: isnull.as_ptr(),
        nrows: 70,
        ..DatumColumn::EMPTY
    };
    let arg = CalendarArg {
        column: &column,
        scalar: 0,
    };
    let mut cache = cache();
    let locals = run(&mut cache, Op::Locals, &arg, 70);
    // A second later each instant lies in the span it lay in, which the
    // cache keeps: nothing is asked.
    let later: Vec<u64> = datums
        .iter()
        .enumerate()
        .map(|(row, &d)| {
            if (7..=9).contains(&row) {
                d
            } else {
                d + USECS as u64
            }
        })
        .collect();
    let later_column = DatumColumn {
        struct_size: size_of::<DatumColumn>(),
        values: later.as_ptr(),
        isnull: isnull.as_ptr(),
        nrows: 70,
        ..DatumColumn::EMPTY
    };
    let later_arg = CalendarArg {
        column: &later_column,
        scalar: 0,
    };
    assert_eq!(run_asking(&mut cache, Op::Locals, &later_arg, 70).1, 0);
    let hours = run(&mut cache, Op::Truncate(HOUR_UNIT), &arg, 70);
    let days = run(&mut cache, Op::Truncate(DAY_UNIT), &arg, 70);
    let dates = run(&mut cache, Op::Dates, &arg, 70);
    for row in 0..70 {
        match row {
            7 => assert_eq!(
                (locals[row], hours[row], dates[row]),
                (Some(i64::MIN), None, Some(i64::from(i32::MIN)))
            ),
            8 => assert_eq!(
                (locals[row], hours[row], dates[row]),
                (Some(i64::MAX), None, Some(i64::from(i32::MAX)))
            ),
            9 => assert_eq!((locals[row], hours[row], dates[row]), (None, None, None)),
            _ => {
                let second = seconds[row];
                let local = second + offset(second);
                assert_eq!(locals[row], Some((local - EPOCH) * USECS), "row {row}");
                let hour = local - local.rem_euclid(3600) - offset(second);
                assert_eq!(hours[row], Some((hour - EPOCH) * USECS), "row {row}");
                let day = (local - EPOCH).div_euclid(DAY);
                assert_eq!(dates[row], Some(day), "row {row}");
                let midnight = day * DAY + EPOCH;
                let start = midnight - offset(midnight - offset(midnight));
                assert_eq!(days[row], Some((start - EPOCH) * USECS), "row {row}");
            }
        }
    }
    // The dates back to their midnights.
    let date_datums: Vec<u64> = dates
        .iter()
        .map(|d| d.unwrap_or(0) as i32 as i64 as u64)
        .collect();
    let date_column = DatumColumn {
        struct_size: size_of::<DatumColumn>(),
        values: date_datums.as_ptr(),
        isnull: isnull.as_ptr(),
        nrows: 70,
        ..DatumColumn::EMPTY
    };
    let midnights = run(
        &mut cache,
        Op::Midnights,
        &CalendarArg {
            column: &date_column,
            scalar: 0,
        },
        70,
    );
    assert_eq!(midnights[10], days[10]);
    assert_eq!(midnights[7], Some(i64::MIN));
}

/// A scalar instant and a scalar date, as `extract` of a constant and the
/// casts pass them; then a reset, after which the cache asks again.
#[test]
fn a_scalar_and_a_reset() {
    let second = EPOCH + 100 * DAY + 10;
    let arg = CalendarArg {
        column: ptr::null(),
        scalar: ((second - EPOCH) * USECS) as u64,
    };
    let mut cache = cache();
    let local = run(&mut cache, Op::Locals, &arg, 1);
    assert_eq!(local, vec![Some((second + 7200 - EPOCH) * USECS)]);
    assert!(cache.words.iter().any(|&word| word != 0));
    let mut status = Status::new();
    // SAFETY: a local cache and status.
    assert_eq!(
        unsafe { tess_zone_reset(&mut *cache, &mut status) },
        Code::Ok
    );
    assert!(cache.words.iter().all(|&word| word == 0));
    let date = CalendarArg {
        column: ptr::null(),
        scalar: 100,
    };
    let midnight = run(&mut cache, Op::Midnights, &date, 1);
    assert_eq!(midnight, vec![Some((100 * DAY - 7200) * USECS)]);
}

/// A null cache, an answer of an unknown kind, a midnight's day past an
/// int4, a negative count of answers: refused as invalid arguments.
#[test]
fn wrong_rounds_are_refused() {
    let mut cache = cache();
    let datum = [0_u64];
    let isnull = [false];
    let column = DatumColumn {
        struct_size: size_of::<DatumColumn>(),
        values: datum.as_ptr(),
        isnull: isnull.as_ptr(),
        nrows: 1,
        ..DatumColumn::EMPTY
    };
    let arg = CalendarArg {
        column: &column,
        scalar: 0,
    };
    let bad = [
        ZoneAnswer {
            kind: 7,
            ..ZoneAnswer::default()
        },
        ZoneAnswer {
            kind: ZONE_MIDNIGHT,
            key: i64::from(i32::MAX) + 1,
            ..ZoneAnswer::default()
        },
    ];
    // One pointer to the cache, copied: each reborrow would retire the last.
    let shared: *mut ZoneCache = &mut *cache;
    for (cache_ptr, answers, nanswers) in [
        (ptr::null_mut(), &bad[..0], 0),
        (shared, &bad[..1], 1),
        (shared, &bad[1..], 1),
        (shared, &bad[..0], -1),
    ] {
        let mut words = [1_u64; 4];
        let rows = Mask {
            nrows: 1,
            bits: words.as_mut_ptr(),
        };
        let mut ready = Mask {
            nrows: 1,
            bits: words[1..].as_mut_ptr(),
        };
        let mut core = Mask {
            nrows: 1,
            bits: words[2..].as_mut_ptr(),
        };
        let mut unknown = Mask {
            nrows: 1,
            bits: words[3..].as_mut_ptr(),
        };
        let mut questions = [ZoneQuestion::default()];
        let mut call = ZoneRound {
            struct_size: size_of::<ZoneRound>(),
            cache: cache_ptr,
            answers: answers.as_ptr(),
            nanswers,
            rows: &rows,
            ready: &mut ready,
            core: &mut core,
            unknown: &mut unknown,
            questions: questions.as_mut_ptr(),
            nquestions: 0,
        };
        let mut locals = [0_i64];
        let mut status = Status::new();
        // SAFETY: a local round and buffers.
        let code = unsafe { tess_zone_locals(&mut call, &arg, locals.as_mut_ptr(), &mut status) };
        assert_eq!(code, Code::InvalidArgument, "{nanswers}");
    }
}
