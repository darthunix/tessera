//! The entry points of the local time of a timestamp with time zone,
//! declared in `include/tessera/calendar.h`: [`tessera_kernels::calendar::zone`]
//! over a cache of the caller's words and the rounds of questions and
//! answers the caller runs.
//!
//! # Arguments
//!
//! A round (`TessZoneRound`) names the cache, every answer of the batch so
//! far, the rows to settle, the three masks of the outcome and room for
//! the questions. The cache must be a valid, initialized `TessZoneCache`
//! (zero bytes are an empty cache) that nothing else accesses during the
//! call; no reference to it outlives the call. The answers must be
//! `nanswers` valid `TessZoneAnswer`s, the questions room for the rows'
//! row count of `TessZoneQuestion`s; the masks valid, of that row count,
//! and apart. The arguments follow the module documentation of
//! [`super::calendar`].

use std::ffi::c_int;
use std::slice;

use anyhow::{Context, Result, bail, ensure};
use tessera_core::RowMaskView;
use tessera_kernels::calendar::zone::{self, Answer, CACHE_WORDS, Cache, Outcome, Question};

use super::calendar::{CalendarArg, unit_of};
use super::mask::Mask;
use super::source::{input, output, selection, slots, with_source};
use super::status::{Code, Status, guard};

/// `TessZoneCache`: the cache's words.
#[repr(C)]
pub struct ZoneCache {
    pub words: [u64; CACHE_WORDS],
}

/// `TessZoneKind`: a question of a span or of a midnight.
pub const ZONE_SPAN: i32 = 0;
pub const ZONE_MIDNIGHT: i32 = 1;

/// `TessZoneQuestion`: what a row asks.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ZoneQuestion {
    /// A Unix second, or a Julian day.
    pub key: i64,
    /// `ZONE_SPAN` or `ZONE_MIDNIGHT`.
    pub kind: i32,
}

/// `TessZoneAnswer`: what the time zone library said.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ZoneAnswer {
    /// The asked second, or the Julian day.
    pub key: i64,
    /// Seconds east of UTC at the second, or west of UTC of the midnight.
    pub offset: i64,
    /// The next transition, when `found` is 1.
    pub boundary: i64,
    /// `ZONE_SPAN` or `ZONE_MIDNIGHT`.
    pub kind: i32,
    /// For a span, `pg_next_dst_boundary`'s result: 1 a next transition at
    /// `boundary`, 0 none, -1 a failure, `offset` then `pg_localtime`'s.
    pub found: i32,
}

/// `TessZoneRound`: one call of a round.
#[repr(C)]
pub struct ZoneRound {
    pub struct_size: usize,
    pub cache: *mut ZoneCache,
    pub answers: *const ZoneAnswer,
    pub nanswers: c_int,
    pub rows: *const Mask,
    pub ready: *mut Mask,
    pub core: *mut Mask,
    pub unknown: *mut Mask,
    pub questions: *mut ZoneQuestion,
    /// Out: the count of distinct questions written.
    pub nquestions: c_int,
}

fn answer_of(answer: &ZoneAnswer) -> Result<Answer> {
    Ok(match (answer.kind, answer.found) {
        (ZONE_SPAN, found) if found > 0 => Answer::Span {
            second: answer.key,
            offset: answer.offset,
            end: Some(answer.boundary),
        },
        (ZONE_SPAN, 0) => Answer::Span {
            second: answer.key,
            offset: answer.offset,
            end: None,
        },
        (ZONE_SPAN, _) => Answer::Second {
            second: answer.key,
            offset: answer.offset,
        },
        (ZONE_MIDNIGHT, _) => Answer::Midnight {
            julian: i32::try_from(answer.key).context("a midnight's Julian day")?,
            offset: answer.offset,
        },
        (kind, _) => bail!("an answer of an unknown kind {kind}"),
    })
}

/// Run one batch function of the module over a round.
///
/// # Safety
///
/// `round` must point to a valid round, as the module documentation says.
unsafe fn with_round(
    round: *mut ZoneRound,
    call: impl FnOnce(&mut Cache<'_>, &[Answer], RowMaskView<'_>, &mut Outcome<'_, '_>) -> Result<()>,
) -> Result<()> {
    // SAFETY: the caller's contract.
    unsafe {
        let round = round.as_mut().context("a null zone round")?;
        ensure!(
            round.struct_size >= size_of::<ZoneRound>(),
            "a zone round smaller than its fields"
        );
        let cache = round.cache.as_mut().context("a null zone cache")?;
        let mut cache = Cache::new(&mut cache.words)?;
        let count = usize::try_from(round.nanswers).context("a negative count of answers")?;
        let given: &[ZoneAnswer] = if count == 0 {
            &[]
        } else {
            ensure!(!round.answers.is_null(), "null answers");
            slice::from_raw_parts(round.answers, count)
        };
        let answers = given.iter().map(answer_of).collect::<Result<Vec<_>>>()?;
        let rows = selection(round.rows)?;
        let nrows = rows.nrows();
        let mut ready = output(round.ready)?;
        let mut core = output(round.core)?;
        let mut unknown = output(round.unknown)?;
        let room = slots(round.questions, nrows)?;
        // A batch's questions on the stack; a larger one allocates.
        let mut stack = [Question::Span(0); 64];
        let mut heap = Vec::new();
        let questions: &mut [Question] = if nrows <= stack.len() {
            &mut stack[..nrows]
        } else {
            heap.resize(nrows, Question::Span(0));
            &mut heap
        };
        let mut out = Outcome {
            ready: &mut ready,
            core: &mut core,
            unknown: &mut unknown,
            questions,
            asked: 0,
        };
        call(&mut cache, &answers, rows, &mut out)?;
        let asked = out.asked;
        for (slot, question) in room.iter_mut().zip(&out.questions[..asked]) {
            slot.write(match *question {
                Question::Span(second) => ZoneQuestion {
                    key: second,
                    kind: ZONE_SPAN,
                },
                Question::Midnight(julian) => ZoneQuestion {
                    key: i64::from(julian),
                    kind: ZONE_MIDNIGHT,
                },
            });
        }
        round.nquestions = c_int::try_from(asked).context("too many questions")?;
        Ok(())
    }
}

/// `tess_zone_reset`: forget the spans (`ZONE_SPAN`) or the midnights
/// (`ZONE_MIDNIGHT`) the cache keeps, the zone changed; the spans' next
/// victim stays.
///
/// # Safety
///
/// `cache` must point to a valid `TessZoneCache` that nothing else accesses
/// during the call; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_zone_reset(
    cache: *mut ZoneCache,
    kind: c_int,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let cache = cache.as_mut().context("a null zone cache")?;
            let mut cache = Cache::new(&mut cache.words)?;
            match kind {
                ZONE_SPAN => cache.reset_spans(),
                ZONE_MIDNIGHT => cache.reset_midnights(),
                _ => bail!("a reset of an unknown kind {kind}"),
            }
            Ok(())
        })
    }
}

/// `tess_zone_locals`: the local times of `instants`' rows into `locals`.
///
/// # Safety
///
/// `round` as the module documentation says; `instants` a valid argument
/// of the rows' row count, `locals` as many writable int8 slots, accessed
/// by nothing else for the call; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_zone_locals(
    round: *mut ZoneRound,
    instants: *const CalendarArg,
    locals: *mut i64,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            with_round(round, |cache, answers, rows, out| {
                let nrows = rows.nrows();
                let values = slots(locals, nrows)?;
                with_source!(input::<i64>(instants, nrows)?, |source| zone::locals(
                    cache, answers, source, rows, values, out
                ))
            })
        })
    }
}

/// `tess_zone_truncate`: `date_trunc(unit, timestamptz)` of `instants`' rows
/// into `values`, from their local times, `locals`, which
/// [`tess_zone_locals`] gave.
///
/// # Safety
///
/// As for [`tess_zone_locals`]; `locals` a valid argument of the rows' row
/// count whose selected non-NULL rows hold their local times, `values` as
/// many writable int8 slots.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_zone_truncate(
    round: *mut ZoneRound,
    unit: c_int,
    instants: *const CalendarArg,
    locals: *const CalendarArg,
    values: *mut i64,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let unit = unit_of(unit)?;
            with_round(round, |cache, answers, rows, out| {
                let nrows = rows.nrows();
                let values = slots(values, nrows)?;
                let locals = input::<i64>(locals, nrows)?;
                with_source!(input::<i64>(instants, nrows)?, |instants| {
                    with_source!(locals, |locals| zone::truncate(
                        cache, answers, unit, instants, locals, rows, values, out
                    ))
                })
            })
        })
    }
}

/// `tess_zone_midnights`: `timestamptz(date)` of `dates`' rows into
/// `values`.
///
/// # Safety
///
/// As for [`tess_zone_locals`], `dates` an argument of dates, `values` as
/// many writable int8 slots.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_zone_midnights(
    round: *mut ZoneRound,
    dates: *const CalendarArg,
    values: *mut i64,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            with_round(round, |cache, answers, rows, out| {
                let nrows = rows.nrows();
                let values = slots(values, nrows)?;
                with_source!(input::<i32>(dates, nrows)?, |source| zone::midnights(
                    cache, answers, source, rows, values, out
                ))
            })
        })
    }
}

/// `tess_zone_dates`: `date(timestamptz)` of `instants`' rows into
/// `values`.
///
/// # Safety
///
/// As for [`tess_zone_locals`], `values` as many writable int4 slots.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_zone_dates(
    round: *mut ZoneRound,
    instants: *const CalendarArg,
    values: *mut i32,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            with_round(round, |cache, answers, rows, out| {
                let nrows = rows.nrows();
                let values = slots(values, nrows)?;
                with_source!(input::<i64>(instants, nrows)?, |source| zone::dates(
                    cache, answers, source, rows, values, out
                ))
            })
        })
    }
}
