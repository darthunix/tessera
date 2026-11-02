//! The local time of a timestamp with time zone, in the session's zone:
//! what `date_trunc`, `extract` and the casts between `date` and
//! `timestamptz` read of an instant.
//!
//! A `timestamptz` is an instant in UTC. Its local time is the instant
//! plus the zone's offset at that second; the offset changes at the zone's
//! transitions, such as the start and the end of summer time. A local
//! day's midnight is an instant again: the local midnight plus the offset
//! the zone gives that local time. Only PostgreSQL's time zone library
//! knows the transitions, and Rust never calls PostgreSQL. So the rules
//! here keep a cache of what the library said ([`Cache`]), in memory the
//! caller owns, and a call over a batch asks for what the cache cannot
//! answer:
//!
//! - a call sorts each selected non-NULL row into one outcome: **ready**,
//!   its value written; **for the core**, which the core's function must
//!   compute (a result out of range, a day before the Julian day 0, a
//!   midnight the zone cannot give, an infinity the operation does not
//!   keep); or **unknown**, which asks a [`Question`];
//! - the caller asks the library the questions and calls again over the
//!   unknown rows with every [`Answer`] it has got in the batch so far; the
//!   call keeps them in the cache, in the order of the rows that need
//!   them, as the library's answers were learned one row at a time.
//!
//! A call never asks a question that an answer it was given covers, so
//! each round adds answers, and a batch of `n` rows needs at most `2n`
//! distinct questions: one span and one midnight a row. Most batches need
//! one call, or two.
//!
//! The cache keeps 32 spans of the zone (the instants up to its next
//! transition, with their offset), replaced in turn, and 1024 midnights by
//! day; a lookup never evicts. Its words are the caller's; zero words are
//! an empty cache.

use std::mem::MaybeUninit;

use anyhow::{Result, ensure};
use tessera_core::{RowMask, RowMaskView, check_rows, ones};

use super::{
    POSTGRES_EPOCH_JDATE, SECS_PER_DAY, TIMESTAMP_END_JULIAN, Truncated, UNIX_EPOCH_JDATE,
    USECS_PER_DAY, USECS_PER_SEC, Unit, is_valid_julian, is_valid_timestamp, julian_to_date, split,
    timestamp_is_infinite, truncate_local,
};
use crate::source::Source;

/// The spans of the zone the cache keeps.
pub const SPANS: usize = 32;
/// The midnights the cache keeps, by day.
pub const MIDNIGHTS: usize = 1024;
/// The header: the count of spans, the last span found, the next victim.
const HEADER: usize = 3;
/// A span: its first second, the second it ends before, its offset.
const SPAN_WORDS: usize = 3;
/// A midnight: its state, its day, its instant.
const MIDNIGHT_WORDS: usize = 3;
/// The words of a cache.
pub const CACHE_WORDS: usize = HEADER + SPANS * SPAN_WORDS + MIDNIGHTS * MIDNIGHT_WORDS;

const MIDNIGHT_BASE: usize = HEADER + SPANS * SPAN_WORDS;
/// A midnight slot's states; zero is empty.
const MIDNIGHT_EMPTY: u64 = 0;
const MIDNIGHT_AT: u64 = 1;
const MIDNIGHT_NONE: u64 = 2;

/// The seconds from the Unix epoch to PostgreSQL's, 2000-01-01.
const EPOCH_SECONDS: i64 = (POSTGRES_EPOCH_JDATE - UNIX_EPOCH_JDATE) as i64 * SECS_PER_DAY;

/// The cache of a zone over the caller's words.
pub struct Cache<'a> {
    words: &'a mut [u64],
}

/// A span of the zone: the seconds from `start` up to `end`, at `offset`
/// seconds east of UTC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Span {
    start: i64,
    end: i64,
    offset: i64,
}

/// What the cache knows of a day's midnight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Midnight {
    Unknown,
    /// The zone gives no midnight within the timestamps.
    None,
    At(i64),
}

impl<'a> Cache<'a> {
    /// The cache over `words`, which must be [`CACHE_WORDS`] long.
    ///
    /// # Errors
    ///
    /// Another count of words, or a header no cache writes: more spans
    /// than the cache keeps, or a last span or a next victim past them.
    pub fn new(words: &'a mut [u64]) -> Result<Self> {
        ensure!(
            words.len() == CACHE_WORDS,
            "a zone cache of {} words, not {CACHE_WORDS}",
            words.len()
        );
        ensure!(
            words[0] <= SPANS as u64 && words[1] < SPANS as u64 && words[2] < SPANS as u64,
            "a damaged zone cache"
        );
        Ok(Self { words })
    }

    /// Forget everything: the zone changed.
    pub fn reset(&mut self) {
        self.words.fill(0);
    }

    /// Forget the spans, the zone changed, but keep the next victim, as
    /// the C code these rules replace kept it across zones.
    pub fn reset_spans(&mut self) {
        let victim = self.words[2];
        self.words[..MIDNIGHT_BASE].fill(0);
        self.words[2] = victim;
    }

    /// Forget the midnights: the zone changed.
    pub fn reset_midnights(&mut self) {
        self.words[MIDNIGHT_BASE..].fill(0);
    }

    fn spans(&self) -> usize {
        self.words[0] as usize
    }

    fn span(&self, index: usize) -> Span {
        let base = HEADER + index * SPAN_WORDS;
        Span {
            start: self.words[base] as i64,
            end: self.words[base + 1] as i64,
            offset: self.words[base + 2] as i64,
        }
    }

    fn set_span(&mut self, index: usize, span: Span) {
        let base = HEADER + index * SPAN_WORDS;
        self.words[base] = span.start as u64;
        self.words[base + 1] = span.end as u64;
        self.words[base + 2] = span.offset as u64;
    }

    /// The offset at a Unix second, if a span holds it: the span found
    /// last first, then each in turn; the one found becomes the last. The
    /// last span is a kept one, or an empty slot, all zeros, which holds
    /// no second.
    fn offset_at(&mut self, second: i64) -> Option<i64> {
        let count = self.spans();
        let last = self.words[1] as usize;
        let holds = |span: Span| span.start <= second && second < span.end;
        if holds(self.span(last)) {
            return Some(self.span(last).offset);
        }
        let found = (0..count).find(|&index| holds(self.span(index)))?;
        self.words[1] = found as u64;
        Some(self.span(found).offset)
    }

    /// Keep the span from `second` up to `end` at `offset`: a kept span of
    /// the same end and offset is the same segment of the zone and starts
    /// earlier now; else the span takes a free slot, or the next victim's.
    fn learn_span(&mut self, second: i64, end: i64, offset: i64) {
        let count = self.spans();
        for index in 0..count {
            let mut span = self.span(index);
            if span.end == end && span.offset == offset {
                span.start = span.start.min(second);
                self.set_span(index, span);
                self.words[1] = index as u64;
                return;
            }
        }
        let slot = if count < SPANS {
            self.words[0] = count as u64 + 1;
            count
        } else {
            let victim = self.words[2] as usize;
            self.words[2] = ((victim + 1) % SPANS) as u64;
            victim
        };
        self.set_span(
            slot,
            Span {
                start: second,
                end,
                offset,
            },
        );
        self.words[1] = slot as u64;
    }

    fn midnight_slot(day: i64) -> usize {
        MIDNIGHT_BASE + (day as u64 % MIDNIGHTS as u64) as usize * MIDNIGHT_WORDS
    }

    fn midnight(&self, julian: i32) -> Midnight {
        let day = i64::from(julian) - i64::from(POSTGRES_EPOCH_JDATE);
        let slot = Self::midnight_slot(day);
        match self.words[slot] {
            state if state != MIDNIGHT_EMPTY && self.words[slot + 1] as i64 == day => {
                if state == MIDNIGHT_AT {
                    Midnight::At(self.words[slot + 2] as i64)
                } else {
                    Midnight::None
                }
            }
            _ => Midnight::Unknown,
        }
    }

    /// Keep a day's midnight, from the offset in seconds west of UTC the
    /// zone gives its local midnight: the instant is the local midnight
    /// plus it, if that is a timestamp.
    fn learn_midnight(&mut self, julian: i32, offset_west: i64) {
        let day = i64::from(julian) - i64::from(POSTGRES_EPOCH_JDATE);
        let instant = day
            .checked_mul(USECS_PER_DAY)
            .zip(offset_west.checked_mul(USECS_PER_SEC))
            .and_then(|(midnight, offset)| midnight.checked_add(offset))
            .filter(|&instant| is_valid_timestamp(instant));
        let slot = Self::midnight_slot(day);
        self.words[slot] = if instant.is_some() {
            MIDNIGHT_AT
        } else {
            MIDNIGHT_NONE
        };
        self.words[slot + 1] = day as u64;
        self.words[slot + 2] = instant.unwrap_or(0) as u64;
    }
}

/// What a row asks of the time zone library. Questions order as the
/// caller asks them: the spans by second, then the midnights by day.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Question {
    /// The offset at a Unix second and the zone's next transition after it
    /// (`pg_next_dst_boundary`).
    Span(i64),
    /// The offset in seconds west of UTC of a Julian day's local midnight
    /// (`DetermineTimeZoneOffset`).
    Midnight(i32),
}

/// What the time zone library said.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    /// At `second` the zone is `offset` seconds east of UTC, up to its
    /// next transition at `end`, or for good when `None`.
    Span {
        second: i64,
        offset: i64,
        end: Option<i64>,
    },
    /// The library could not find the span of `second` (a failure its
    /// sources call unreachable); the offset of that one second
    /// (`pg_localtime`).
    Second { second: i64, offset: i64 },
    /// The local midnight of the Julian day `julian` is `offset` seconds
    /// west of UTC.
    Midnight { julian: i32, offset: i64 },
}

impl Answer {
    /// The span an answer of a second gives: the second it starts at, the
    /// one it ends before, its offset; `None` for a midnight.
    fn span(self) -> Option<Span> {
        match self {
            Self::Span {
                second,
                offset,
                end,
            } => Some(Span {
                start: second,
                end: end.unwrap_or(i64::MAX),
                offset,
            }),
            Self::Second { second, offset } => Some(Span {
                start: second,
                end: second.saturating_add(1),
                offset,
            }),
            Self::Midnight { .. } => None,
        }
    }
}

/// The cache and the answers of a call.
struct Zone<'c, 'w, 'a> {
    cache: &'c mut Cache<'w>,
    answers: &'a [Answer],
}

impl Zone<'_, '_, '_> {
    /// The offset at a Unix second: from the cache, else from an answer
    /// that covers the second, which the cache then keeps.
    fn offset(&mut self, second: i64) -> Result<i64, Question> {
        if let Some(offset) = self.cache.offset_at(second) {
            return Ok(offset);
        }
        let span = self
            .answers
            .iter()
            .filter_map(|answer| answer.span())
            .find(|span| span.start <= second && second < span.end)
            .ok_or(Question::Span(second))?;
        self.cache.learn_span(span.start, span.end, span.offset);
        Ok(span.offset)
    }

    /// An instant's local time, as `timestamp2tm` reads it: the instant
    /// plus the offset at its second, rounded down.
    fn local(&mut self, instant: i64) -> Result<i64, Question> {
        let second = instant.div_euclid(USECS_PER_SEC) + EPOCH_SECONDS;
        let offset = self.offset(second)?;
        // A valid instant's local time stays far from the integer's ends.
        Ok(instant.wrapping_add(offset * USECS_PER_SEC))
    }

    /// The instant of a Julian day's local midnight, as `date2timestamptz`
    /// and `timestamptz_trunc` make it; `Ok(None)` for a day the core's
    /// function must take: before the Julian day 0, past the Julian day
    /// routines, or a midnight out of the timestamps.
    fn midnight(&mut self, julian: i32) -> Result<Option<i64>, Question> {
        match self.cache.midnight(julian) {
            Midnight::At(instant) => return Ok(Some(instant)),
            Midnight::None => return Ok(None),
            Midnight::Unknown => {}
        }
        let (year, month, _) = julian_to_date(julian);
        if julian < 0 || !is_valid_julian(year, month) {
            return Ok(None);
        }
        let offset = self
            .answers
            .iter()
            .find_map(|answer| match *answer {
                Answer::Midnight {
                    julian: day,
                    offset,
                } if day == julian => Some(offset),
                _ => None,
            })
            .ok_or(Question::Midnight(julian))?;
        self.cache.learn_midnight(julian, offset);
        Ok(match self.cache.midnight(julian) {
            Midnight::At(instant) => Some(instant),
            _ => None,
        })
    }
}

/// A row's outcome.
enum Step<T> {
    Ready(T),
    Core,
    Ask(Question),
}

impl<T> From<Result<Option<T>, Question>> for Step<T> {
    fn from(result: Result<Option<T>, Question>) -> Self {
        match result {
            Ok(Some(value)) => Self::Ready(value),
            Ok(None) => Self::Core,
            Err(question) => Self::Ask(question),
        }
    }
}

/// The masks and the questions of a call: `ready` and `core` gain the rows
/// the call settles and keep the others; `unknown` gets every word of the
/// rows written; `questions` the distinct questions, sorted, and
/// [`Outcome::asked`] their count.
pub struct Outcome<'a, 'b> {
    pub ready: &'a mut RowMask<'b>,
    pub core: &'a mut RowMask<'b>,
    pub unknown: &'a mut RowMask<'b>,
    pub questions: &'a mut [Question],
    pub asked: usize,
}

/// Every selected row through `step`, in row order: a `None` row is NULL
/// and gets no outcome.
fn settle<T>(
    cache: &mut Cache<'_>,
    answers: &[Answer],
    rows: RowMaskView<'_>,
    values: &mut [MaybeUninit<T>],
    out: &mut Outcome<'_, '_>,
    mut step: impl FnMut(&mut Zone<'_, '_, '_>, usize) -> Option<Step<T>>,
) -> Result<()> {
    check_rows(
        rows.nrows(),
        &[
            values.len(),
            out.ready.as_view().nrows(),
            out.core.as_view().nrows(),
            out.unknown.as_view().nrows(),
        ],
    )?;
    ensure!(
        out.questions.len() >= rows.nrows(),
        "room for {} questions, not {}",
        rows.nrows(),
        out.questions.len()
    );
    for answer in answers {
        if let Some(span) = answer.span() {
            ensure!(
                span.start < span.end,
                "an answer of a span that ends at its second"
            );
        }
    }
    let mut zone = Zone { cache, answers };
    let mut asked = 0;
    for word in 0..rows.nrows().div_ceil(64) {
        let (mut ready, mut core, mut unknown) = (0, 0, 0);
        for bit in ones(rows.word(word)) {
            let row = word * 64 + bit;
            match step(&mut zone, row) {
                None => {}
                Some(Step::Ready(value)) => {
                    values[row].write(value);
                    ready |= 1 << bit;
                }
                Some(Step::Core) => core |= 1 << bit,
                Some(Step::Ask(question)) => {
                    out.questions[asked] = question;
                    asked += 1;
                    unknown |= 1 << bit;
                }
            }
        }
        out.ready
            .set_word(word, out.ready.as_view().word(word) | ready)?;
        out.core
            .set_word(word, out.core.as_view().word(word) | core)?;
        out.unknown.set_word(word, unknown)?;
    }
    let questions = &mut out.questions[..asked];
    questions.sort_unstable();
    let mut distinct = 0;
    for index in 0..asked {
        if index == 0 || questions[index] != questions[distinct - 1] {
            questions[distinct] = questions[index];
            distinct += 1;
        }
    }
    out.asked = distinct;
    Ok(())
}

/// The local times of `instants`' selected non-NULL rows into `locals`,
/// for `extract`: an infinity kept, every other row ready or unknown.
///
/// # Errors
///
/// Dimensions that disagree, too little room for questions, or an answer
/// of a span that ends at its second.
pub fn locals(
    cache: &mut Cache<'_>,
    answers: &[Answer],
    instants: &impl Source<i64>,
    rows: RowMaskView<'_>,
    locals: &mut [MaybeUninit<i64>],
    out: &mut Outcome<'_, '_>,
) -> Result<()> {
    settle(cache, answers, rows, locals, out, |zone, row| {
        let instant = instants.get(row)?;
        Some(if timestamp_is_infinite(instant) {
            Step::Ready(instant)
        } else {
            zone.local(instant).map(Some).into()
        })
    })
}

/// `date_trunc(unit, instant)` in the zone, as `timestamptz_trunc_internal`
/// computes it, from the instants and their local times, which
/// [`locals`] gave before: the local time truncated by the calendar; a
/// unit below a day keeps the instant's own offset, a day or more takes
/// the local midnight of its first day. An infinity, a local time the
/// calendar leaves, a midnight the zone cannot give or a result out of
/// the timestamps is for the core. Only midnights are asked, so that they
/// are learned in the order of the rows, after every local time, as the
/// C code learned them.
///
/// # Errors
///
/// As for [`locals`].
// The cache, the answers, the unit, two arguments, the rows, the values
// and the outcome, each its own.
#[allow(clippy::too_many_arguments)]
pub fn truncate(
    cache: &mut Cache<'_>,
    answers: &[Answer],
    unit: Unit,
    instants: &impl Source<i64>,
    locals: &impl Source<i64>,
    rows: RowMaskView<'_>,
    values: &mut [MaybeUninit<i64>],
    out: &mut Outcome<'_, '_>,
) -> Result<()> {
    settle(cache, answers, rows, values, out, |zone, row| {
        let instant = instants.get(row)?;
        if timestamp_is_infinite(instant) {
            return Some(Step::Core);
        }
        let local = locals.get(row)?;
        Some(match truncate_local(local, unit) {
            None => Step::Core,
            Some(Truncated::Time(time)) => {
                let result = time - (local - instant);
                if is_valid_timestamp(result) {
                    Step::Ready(result)
                } else {
                    Step::Core
                }
            }
            Some(Truncated::Day(julian)) => zone.midnight(julian).into(),
        })
    })
}

/// `timestamptz(date)`, a date's local midnight: an infinite date the
/// infinite instant; a date past the timestamps, before the Julian day 0
/// or whose midnight the zone cannot give is for the core.
///
/// # Errors
///
/// As for [`locals`].
pub fn midnights(
    cache: &mut Cache<'_>,
    answers: &[Answer],
    dates: &impl Source<i32>,
    rows: RowMaskView<'_>,
    values: &mut [MaybeUninit<i64>],
    out: &mut Outcome<'_, '_>,
) -> Result<()> {
    settle(cache, answers, rows, values, out, |zone, row| {
        let date = dates.get(row)?;
        Some(match date {
            super::DATE_NOBEGIN => Step::Ready(super::TIMESTAMP_NOBEGIN),
            super::DATE_NOEND => Step::Ready(super::TIMESTAMP_NOEND),
            _ if date >= TIMESTAMP_END_JULIAN - POSTGRES_EPOCH_JDATE => Step::Core,
            _ => zone.midnight(date + POSTGRES_EPOCH_JDATE).into(),
        })
    })
}

/// `date(timestamptz)`, the day of an instant's local time, rounded down:
/// an infinity the infinite date; a day before the Julian day 0 is for
/// the core.
///
/// # Errors
///
/// As for [`locals`].
pub fn dates(
    cache: &mut Cache<'_>,
    answers: &[Answer],
    instants: &impl Source<i64>,
    rows: RowMaskView<'_>,
    values: &mut [MaybeUninit<i32>],
    out: &mut Outcome<'_, '_>,
) -> Result<()> {
    settle(cache, answers, rows, values, out, |zone, row| {
        let instant = instants.get(row)?;
        Some(match instant {
            super::TIMESTAMP_NOBEGIN => Step::Ready(super::DATE_NOBEGIN),
            super::TIMESTAMP_NOEND => Step::Ready(super::DATE_NOEND),
            _ => match zone.local(instant) {
                Err(question) => Step::Ask(question),
                Ok(local) => {
                    let day = split(local).0;
                    if day + i64::from(POSTGRES_EPOCH_JDATE) >= 0 {
                        Step::Ready(day as i32)
                    } else {
                        Step::Core
                    }
                }
            },
        })
    })
}
