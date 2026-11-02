#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test reports a failure by panicking"
)]
#![deny(unsafe_code)]

//! The rules of local time against model zones: each a list of
//! transitions. A model host answers the module's questions from the
//! model; a reference computes each result straight from the model, with
//! no cache. The rounds of a batch run as the C caller runs them: every
//! answer of the batch so far passed to each call. A second model, of the
//! C code these rules replace, learns one row at a time; the module asks
//! the host no more often than it, from the same cache, and keeps the
//! same spans after a batch where it evicts nothing a later row read.

use std::cell::Cell;
use std::mem::MaybeUninit;

use anyhow::Result;
use proptest::prelude::*;
use tessera_core::{RowMask, RowMaskView};
use tessera_kernels::calendar::zone::{
    self, Answer, CACHE_WORDS, Cache, MIDNIGHTS, Outcome, Question, SPANS,
};
use tessera_kernels::calendar::{
    DATE_NOBEGIN, DATE_NOEND, POSTGRES_EPOCH_JDATE, SECS_PER_DAY, Source, TIMESTAMP_NOBEGIN,
    TIMESTAMP_NOEND, Truncated, UNIX_EPOCH_JDATE, USECS_PER_DAY, USECS_PER_SEC, Unit,
    is_valid_julian, is_valid_timestamp, julian_to_date, split, truncate_local,
};
use tessera_testing::{flags, property, words};

const EPOCH_SECONDS: i64 = (POSTGRES_EPOCH_JDATE - UNIX_EPOCH_JDATE) as i64 * SECS_PER_DAY;
const TIMESTAMP_END_JULIAN: i32 = 109_203_528;
const MIN_TIMESTAMP: i64 = -211_813_488_000_000_000;
const END_TIMESTAMP: i64 = 9_223_371_331_200_000_000;

const UNITS: [Unit; 13] = [
    Unit::Microsecond,
    Unit::Millisecond,
    Unit::Second,
    Unit::Minute,
    Unit::Hour,
    Unit::Day,
    Unit::Week,
    Unit::Month,
    Unit::Quarter,
    Unit::Year,
    Unit::Decade,
    Unit::Century,
    Unit::Millennium,
];

/// A zone: its offset (seconds east of UTC) before the first transition,
/// and its transitions, each a Unix second and the offset from it on.
#[derive(Clone, Debug)]
struct Model {
    initial: i64,
    transitions: Vec<(i64, i64)>,
    /// A divisor of the seconds whose span the library fails to find; 0
    /// for none.
    failing: i64,
}

impl Model {
    fn offset(&self, second: i64) -> i64 {
        let after = self.transitions.partition_point(|&(at, _)| at <= second);
        if after == 0 {
            self.initial
        } else {
            self.transitions[after - 1].1
        }
    }

    fn next(&self, second: i64) -> Option<i64> {
        let after = self.transitions.partition_point(|&(at, _)| at <= second);
        self.transitions.get(after).map(|&(at, _)| at)
    }

    /// The offset west of a Julian day's local midnight: the zone's at the
    /// midnight read as UTC less its offset there, as a gap or a fold
    /// might resolve it.
    fn west(&self, julian: i32) -> i64 {
        let midnight =
            (i64::from(julian) - i64::from(POSTGRES_EPOCH_JDATE)) * SECS_PER_DAY + EPOCH_SECONDS;
        -self.offset(midnight - self.offset(midnight))
    }

    fn fails(&self, second: i64) -> bool {
        self.failing > 0 && second % self.failing == 0
    }

    fn answer(&self, question: Question) -> Answer {
        match question {
            Question::Span(second) if self.fails(second) => Answer::Second {
                second,
                offset: self.offset(second),
            },
            Question::Span(second) => Answer::Span {
                second,
                offset: self.offset(second),
                end: self.next(second),
            },
            Question::Midnight(julian) => Answer::Midnight {
                julian,
                offset: self.west(julian),
            },
        }
    }

    /// The answers to sorted questions; with `skip`, as C asks them: a span
    /// question before the end of the last span answer is not asked.
    fn answers(&self, questions: &[Question], skip: bool, asked: &Cell<usize>) -> Vec<Answer> {
        let mut answers = Vec::new();
        let mut covered_to: Option<i64> = None;
        for &question in questions {
            if let (true, Question::Span(second), Some(end)) = (skip, question, covered_to)
                && second < end
            {
                continue;
            }
            let answer = self.answer(question);
            asked.set(asked.get() + 1);
            covered_to = match answer {
                Answer::Span { end, .. } => Some(end.unwrap_or(i64::MAX)),
                Answer::Second { second, .. } => Some(second + 1),
                Answer::Midnight { .. } => covered_to,
            };
            answers.push(answer);
        }
        answers
    }

    // The reference, straight from the model.

    fn local(&self, instant: i64) -> i64 {
        instant + self.offset(instant.div_euclid(USECS_PER_SEC) + EPOCH_SECONDS) * USECS_PER_SEC
    }

    fn midnight(&self, julian: i32) -> Option<i64> {
        let (year, month, _) = julian_to_date(julian);
        if julian < 0 || !is_valid_julian(year, month) {
            return None;
        }
        let day = i64::from(julian) - i64::from(POSTGRES_EPOCH_JDATE);
        let instant = i128::from(day) * i128::from(USECS_PER_DAY)
            + i128::from(self.west(julian)) * i128::from(USECS_PER_SEC);
        i64::try_from(instant)
            .ok()
            .filter(|&instant| is_valid_timestamp(instant))
    }

    fn reference(&self, op: Op, value: i64) -> Option<i64> {
        match op {
            Op::Locals if value == TIMESTAMP_NOBEGIN || value == TIMESTAMP_NOEND => Some(value),
            Op::Locals => Some(self.local(value)),
            Op::Truncate(_) if value == TIMESTAMP_NOBEGIN || value == TIMESTAMP_NOEND => None,
            Op::Truncate(unit) => {
                let local = self.local(value);
                match truncate_local(local, unit)? {
                    Truncated::Time(time) => {
                        Some(time - (local - value)).filter(|&r| is_valid_timestamp(r))
                    }
                    Truncated::Day(julian) => self.midnight(julian),
                }
            }
            Op::Dates => match value {
                TIMESTAMP_NOBEGIN => Some(i64::from(DATE_NOBEGIN)),
                TIMESTAMP_NOEND => Some(i64::from(DATE_NOEND)),
                _ => {
                    let day = split(self.local(value)).0;
                    (day + i64::from(POSTGRES_EPOCH_JDATE) >= 0).then_some(day)
                }
            },
            Op::Midnights => match value as i32 {
                DATE_NOBEGIN => Some(TIMESTAMP_NOBEGIN),
                DATE_NOEND => Some(TIMESTAMP_NOEND),
                date if date >= TIMESTAMP_END_JULIAN - POSTGRES_EPOCH_JDATE => None,
                date => self.midnight(date + POSTGRES_EPOCH_JDATE),
            },
        }
    }
}

/// A column of a test: a value a row, `None` for NULL.
struct Column<T>(Vec<Option<T>>);

impl<T: Copy> Source<T> for Column<T> {
    fn get(&self, row: usize) -> Option<T> {
        self.0[row]
    }
}

/// One operation of the module.
#[derive(Clone, Copy, Debug)]
enum Op {
    Locals,
    Truncate(Unit),
    Dates,
    Midnights,
}

#[allow(unsafe_code)]
fn written<T: Copy>(slot: &MaybeUninit<T>) -> T {
    // SAFETY: every slot was created initialized and the module only
    // writes initialized values.
    unsafe { slot.assume_init() }
}

/// A run of a batch: each row's result (`Some(None)` for the core, `None`
/// for a NULL or unselected row), the questions the host answered, the
/// calls.
struct Run {
    results: Vec<Option<Option<i64>>>,
    asked: usize,
    calls: usize,
}

/// Run an operation in rounds, as C runs it, until no row is unknown:
/// `date_trunc` in two stages, the local times and then the truncation.
fn run(
    words_: &mut [u64],
    model: &Model,
    op: Op,
    values: &[Option<i64>],
    selected: &[bool],
    skip: bool,
) -> Result<Run> {
    let Op::Truncate(unit) = op else {
        return rounds(words_, model, op, values, None, selected, skip);
    };
    let locals = rounds(words_, model, Op::Locals, values, None, selected, skip)?;
    let local_values: Vec<Option<i64>> = locals.results.iter().map(|r| r.flatten()).collect();
    let mut run = rounds(
        words_,
        model,
        Op::Truncate(unit),
        values,
        Some(&local_values),
        selected,
        skip,
    )?;
    run.asked += locals.asked;
    run.calls += locals.calls;
    Ok(run)
}

/// What a value slot holds until the module writes it.
const SENTINEL: i64 = 0x5EED;

/// One stage's rounds.
fn rounds(
    words_: &mut [u64],
    model: &Model,
    op: Op,
    values: &[Option<i64>],
    locals: Option<&[Option<i64>]>,
    selected: &[bool],
    skip: bool,
) -> Result<Run> {
    let nrows = values.len();
    let selection = words(selected);
    let (mut ready_words, mut core_words) =
        (vec![0; nrows.div_ceil(64)], vec![0; nrows.div_ceil(64)]);
    let mut round_rows = selection.clone();
    let mut answers: Vec<Answer> = Vec::new();
    let asked = Cell::new(0);
    let mut out64 = vec![MaybeUninit::new(SENTINEL); nrows];
    let mut out32 = vec![MaybeUninit::new(SENTINEL as i32); nrows];
    let local_column = Column(locals.unwrap_or(&[]).to_vec());
    let mut questions = vec![Question::Span(0); nrows];
    let instants = Column(values.to_vec());
    let dates = Column(values.iter().map(|v| v.map(|d| d as i32)).collect());
    let mut calls = 0;
    loop {
        calls += 1;
        assert!(calls <= 2 * nrows + 2, "{op:?}: the rounds do not end");
        // Words left dirty: a call writes every word of its unknown rows.
        let mut unknown_words = words(&vec![true; nrows]);
        let rows = RowMaskView::try_new(nrows, &round_rows)?;
        let mut ready = RowMask::try_new(nrows, &mut ready_words)?;
        let mut core = RowMask::try_new(nrows, &mut core_words)?;
        let mut unknown = RowMask::try_new(nrows, &mut unknown_words)?;
        let mut out = Outcome {
            ready: &mut ready,
            core: &mut core,
            unknown: &mut unknown,
            questions: &mut questions,
            asked: 0,
        };
        let mut cache = Cache::new(words_)?;
        match op {
            Op::Locals => {
                zone::locals(&mut cache, &answers, &instants, rows, &mut out64, &mut out)?
            }
            Op::Truncate(unit) => zone::truncate(
                &mut cache,
                &answers,
                unit,
                &instants,
                &local_column,
                rows,
                &mut out64,
                &mut out,
            )?,
            Op::Dates => zone::dates(&mut cache, &answers, &instants, rows, &mut out32, &mut out)?,
            Op::Midnights => {
                zone::midnights(&mut cache, &answers, &dates, rows, &mut out64, &mut out)?;
            }
        }
        let count = out.asked;
        if count == 0 {
            break;
        }
        assert!(
            questions[..count].windows(2).all(|pair| pair[0] < pair[1]),
            "questions sorted and distinct"
        );
        for question in &questions[..count] {
            // No answer given covers a question asked.
            assert!(
                !answers.iter().any(|answer| covers(*answer, *question)),
                "{op:?}: {question:?} asked again"
            );
        }
        answers.extend(model.answers(&questions[..count], skip, &asked));
        round_rows = unknown_words;
    }
    let ready = RowMaskView::try_new(nrows, &ready_words)?;
    let core = RowMaskView::try_new(nrows, &core_words)?;
    let results = (0..nrows)
        .map(|row| {
            let (is_ready, is_core) = (ready.contains(row).unwrap(), core.contains(row).unwrap());
            assert!(
                !(is_ready && is_core),
                "row {row} both ready and for the core"
            );
            let value = match op {
                Op::Dates => i64::from(written(&out32[row])),
                _ => written(&out64[row]),
            };
            // A slot not ready is left as it was.
            assert!(
                is_ready || value == SENTINEL,
                "{op:?} row {row} written, not ready"
            );
            (is_ready || is_core).then_some(is_ready.then_some(value))
        })
        .collect();
    Ok(Run {
        results,
        asked: asked.get(),
        calls,
    })
}

fn covers(answer: Answer, question: Question) -> bool {
    match (answer, question) {
        (Answer::Span { second, end, .. }, Question::Span(asked)) => {
            second <= asked && asked < end.unwrap_or(i64::MAX)
        }
        (Answer::Second { second, .. }, Question::Span(asked)) => second == asked,
        (Answer::Midnight { julian, .. }, Question::Midnight(asked)) => julian == asked,
        _ => false,
    }
}

/// The C code these rules replace: 32 spans replaced in turn, 1024
/// midnights by day, learned one row at a time, a failing span and a
/// missing midnight not kept.
#[derive(Clone, Default)]
struct Today {
    spans: Vec<(i64, i64, i64)>,
    last: usize,
    victim: usize,
    midnights: Vec<Option<(i64, i64)>>,
    asked: usize,
}

impl Today {
    fn new() -> Self {
        Self {
            midnights: vec![None; MIDNIGHTS],
            ..Self::default()
        }
    }

    fn offset(&mut self, model: &Model, second: i64) -> i64 {
        let holds = |span: &(i64, i64, i64)| span.0 <= second && second < span.1;
        if self.last < self.spans.len() && holds(&self.spans[self.last]) {
            return self.spans[self.last].2;
        }
        if let Some(index) = self.spans.iter().position(holds) {
            self.last = index;
            return self.spans[index].2;
        }
        self.asked += 1;
        if model.fails(second) {
            return model.offset(second);
        }
        let (offset, end) = (model.offset(second), model.next(second).unwrap_or(i64::MAX));
        if let Some(index) = self.spans.iter().position(|s| s.1 == end && s.2 == offset) {
            self.spans[index].0 = self.spans[index].0.min(second);
            self.last = index;
            return offset;
        }
        let slot = if self.spans.len() < SPANS {
            self.spans.push((second, end, offset));
            self.spans.len() - 1
        } else {
            let slot = self.victim % SPANS;
            self.victim += 1;
            self.spans[slot] = (second, end, offset);
            slot
        };
        self.last = slot;
        offset
    }
}

/// A random zone: up to 80 transitions, offsets up to 15 hours either side
/// in steps of a quarter hour, between 1900 and 2100 or over the whole
/// range of the timestamps, and now and then a library that fails.
fn models() -> impl Strategy<Value = Model> {
    let offset = (-60_i64..=60).prop_map(|quarters| quarters * 900);
    let span = prop_oneof![
        Just((-3_155_673_600_i64, 4_102_444_800_i64)),
        Just((-210_866_803_200_i64, 9_214_646_400_000_i64)),
    ];
    (
        span,
        offset.clone(),
        prop::collection::vec((any::<u64>(), offset), 0..80),
        prop_oneof![3 => Just(0_i64), 1 => 7_i64..2000],
    )
        .prop_map(|((from, to), initial, raw, failing)| {
            let mut transitions: Vec<(i64, i64)> = raw
                .into_iter()
                .map(|(r, offset)| (from + (r % (to - from) as u64) as i64, offset))
                .collect();
            transitions.sort_unstable();
            transitions.dedup_by_key(|t| t.0);
            Model {
                initial,
                transitions,
                failing,
            }
        })
}

/// Instants near the model's transitions, anywhere in the timestamps, at
/// their ends, and infinities; NULLs.
fn batch(model: &Model) -> impl Strategy<Value = (Vec<Option<i64>>, Vec<bool>)> + use<> {
    let near: Vec<i64> = model
        .transitions
        .iter()
        .map(|&(at, _)| (at - EPOCH_SECONDS) * USECS_PER_SEC)
        .collect();
    // A transition's own second and those either side, where the end of a
    // span decides.
    let seconds = if near.is_empty() {
        Just(0_i64).boxed()
    } else {
        (prop::sample::select(near.clone()), -1_i64..=1)
            .prop_map(|(at, step)| {
                (at + step * USECS_PER_SEC).clamp(MIN_TIMESTAMP, END_TIMESTAMP - 1)
            })
            .boxed()
    };
    let instant = prop_oneof![
        2 => seconds,
        4 => if near.is_empty() { Just(0_i64).boxed() } else {
            (prop::sample::select(near), -2 * USECS_PER_DAY..2 * USECS_PER_DAY)
                .prop_map(|(at, delta)| (at + delta).clamp(MIN_TIMESTAMP, END_TIMESTAMP - 1))
                .boxed()
        },
        3 => (MIN_TIMESTAMP..END_TIMESTAMP).boxed(),
        1 => prop::sample::select(vec![
            MIN_TIMESTAMP,
            MIN_TIMESTAMP + 1,
            END_TIMESTAMP - 1,
            TIMESTAMP_NOBEGIN,
            TIMESTAMP_NOEND,
        ])
        .boxed(),
    ];
    (1_usize..=140).prop_flat_map(move |nrows| {
        (
            prop::collection::vec(prop::option::weighted(0.9, instant.clone()), nrows),
            flags(nrows),
        )
    })
}

/// A date of the same batch: the day of an instant, the ends of the dates
/// and of the timestamps, infinities.
fn as_date(value: i64) -> i64 {
    match value {
        TIMESTAMP_NOBEGIN => i64::from(DATE_NOBEGIN),
        TIMESTAMP_NOEND => i64::from(DATE_NOEND),
        _ if value % 7 == 0 => 2_145_031_948,
        _ if value % 11 == 0 => i64::from(TIMESTAMP_END_JULIAN - POSTGRES_EPOCH_JDATE),
        _ => split(value).0,
    }
}

/// Every operation, with both hosts, matches the reference; the rounds
/// end; the skipping host is asked no more often than the C code would
/// ask, from the same empty cache.
#[test]
fn every_operation_follows_the_model() {
    let cases = models().prop_flat_map(|model| {
        let batch = batch(&model);
        (Just(model), batch)
    });
    property(cases, |(model, (instants, selected))| -> Result<()> {
        let dates: Vec<Option<i64>> = instants.iter().map(|v| v.map(as_date)).collect();
        let mut ops = vec![Op::Locals, Op::Dates, Op::Midnights];
        ops.extend(UNITS.map(Op::Truncate));
        for op in ops {
            let values = if matches!(op, Op::Midnights) {
                &dates
            } else {
                &instants
            };
            for skip in [false, true] {
                let mut words_ = vec![0_u64; CACHE_WORDS];
                let run = run(&mut words_, &model, op, values, &selected, skip)?;
                for (row, result) in run.results.iter().enumerate() {
                    let expected = (selected[row] && values[row].is_some())
                        .then(|| model.reference(op, values[row].unwrap()));
                    assert_eq!(
                        *result, expected,
                        "{op:?} skip {skip} row {row} {:?}",
                        values[row]
                    );
                }
                // A stage takes one call, or two when it asks; date_trunc
                // has two stages.
                let most = if matches!(op, Op::Truncate(_)) { 4 } else { 2 };
                assert!(run.calls <= most, "{op:?}: {} calls", run.calls);
                if skip {
                    // The C code: the local times of every row first, then
                    // the midnights row by row.
                    let mut today = Today::new();
                    let chosen = || {
                        values
                            .iter()
                            .enumerate()
                            .filter_map(|(row, v)| v.filter(|_| selected[row]))
                    };
                    if matches!(op, Op::Truncate(_)) {
                        chosen().for_each(|v| today_row(&mut today, &model, Op::Locals, v));
                    }
                    chosen().for_each(|v| today_row(&mut today, &model, op, v));
                    assert!(
                        run.asked <= today.asked,
                        "{op:?}: asked {} where today asks {}",
                        run.asked,
                        today.asked
                    );
                }
            }
        }
        Ok(())
    });
}

/// The C code's steps for one row: its spans, then its midnight.
fn today_row(today: &mut Today, model: &Model, op: Op, value: i64) {
    let midnight = |today: &mut Today, julian: i32| {
        let day = i64::from(julian) - i64::from(POSTGRES_EPOCH_JDATE);
        let slot = (day as u64 % MIDNIGHTS as u64) as usize;
        if today.midnights[slot].is_some_and(|(kept, _)| kept == day) {
            return;
        }
        let (year, month, _) = julian_to_date(julian);
        if julian < 0 || !is_valid_julian(year, month) {
            return;
        }
        today.asked += 1;
        if let Some(instant) = model.midnight(julian) {
            today.midnights[slot] = Some((day, instant));
        }
    };
    let infinite = value == TIMESTAMP_NOBEGIN || value == TIMESTAMP_NOEND;
    match op {
        Op::Locals | Op::Dates if !infinite => {
            today.offset(model, value.div_euclid(USECS_PER_SEC) + EPOCH_SECONDS);
        }
        Op::Truncate(unit) if !infinite => {
            // The local time is known: its span was asked before.
            if let Some(Truncated::Day(julian)) = truncate_local(model.local(value), unit) {
                midnight(today, julian);
            }
        }
        Op::Midnights => {
            let date = value as i32;
            if date != DATE_NOBEGIN
                && date != DATE_NOEND
                && date < TIMESTAMP_END_JULIAN - POSTGRES_EPOCH_JDATE
            {
                midnight(today, date + POSTGRES_EPOCH_JDATE);
            }
        }
        _ => {}
    }
}

/// The spans kept in the cache's words, in slot order: start, end, offset.
fn spans(words_: &[u64]) -> Vec<(i64, i64, i64)> {
    (0..words_[0] as usize)
        .map(|slot| {
            let base = 3 + slot * 3;
            (
                words_[base] as i64,
                words_[base + 1] as i64,
                words_[base + 2] as i64,
            )
        })
        .collect()
}

/// A zone of a transition every 1000 seconds, its offset the transition's
/// number in hours.
fn steps(count: i64) -> Model {
    Model {
        initial: 0,
        transitions: (1..=count).map(|n| (n * 1000, n * 3600)).collect(),
        failing: 0,
    }
}

fn at_second(second: i64) -> i64 {
    (second - EPOCH_SECONDS) * USECS_PER_SEC
}

/// From the same cache, a batch that evicts nothing it read leaves the
/// spans the C code would leave, in its slots: rows of one span in
/// descending order, then of three spans out of order.
#[test]
fn the_spans_are_kept_as_the_c_code_keeps_them() {
    let model = steps(40);
    for seconds in [
        vec![1900_i64, 1500, 1200],
        vec![3500, 1100, 2700, 1300, 3900, 2100],
    ] {
        let values: Vec<Option<i64>> = seconds.iter().map(|&s| Some(at_second(s))).collect();
        let selected = vec![true; values.len()];
        let mut words_ = vec![0_u64; CACHE_WORDS];
        run(&mut words_, &model, Op::Locals, &values, &selected, true).unwrap();
        let mut today = Today::new();
        for &second in &seconds {
            today.offset(&model, second);
        }
        assert_eq!(spans(&words_), today.spans, "{seconds:?}");
        // The same rows again: the cache answers them all, in one call.
        let again = run(&mut words_, &model, Op::Locals, &values, &selected, true).unwrap();
        assert_eq!((again.asked, again.calls), (0, 1));
    }
    // Two batches of one span, the second's row before the first's: the
    // span starts earlier, in its slot, as the C code joins it.
    let mut words_ = vec![0_u64; CACHE_WORDS];
    let mut today = Today::new();
    for second in [1900_i64, 1500] {
        run(
            &mut words_,
            &model,
            Op::Locals,
            &[Some(at_second(second))],
            &[true],
            true,
        )
        .unwrap();
        today.offset(&model, second);
    }
    assert_eq!(spans(&words_), vec![(1500, 2000, 3600)]);
    assert_eq!(spans(&words_), today.spans);
}

/// A second that a transition starts belongs to the new span, the one
/// before to the old: a kept span ends before its transition.
#[test]
fn a_span_ends_before_its_transition() {
    let model = steps(3);
    let mut words_ = vec![0_u64; CACHE_WORDS];
    run(
        &mut words_,
        &model,
        Op::Locals,
        &[Some(at_second(1500))],
        &[true],
        true,
    )
    .unwrap();
    for second in [1999, 2000, 2001] {
        let row = [Some(at_second(second))];
        let result = run(&mut words_, &model, Op::Locals, &row, &[true], true).unwrap();
        assert_eq!(
            result.results,
            vec![Some(Some(model.local(at_second(second))))],
            "{second}"
        );
    }
}

/// The victims of a full cache in turn, as the C code takes them: span 32
/// replaces slot 0, span 33 slot 1.
#[test]
fn a_full_cache_replaces_its_slots_in_turn() {
    let model = steps(40);
    let mut words_ = vec![0_u64; CACHE_WORDS];
    let mut today = Today::new();
    let first: Vec<Option<i64>> = (0..32).map(|n| Some(at_second(n * 1000 + 500))).collect();
    run(&mut words_, &model, Op::Locals, &first, &[true; 32], true).unwrap();
    for n in 0..32 {
        today.offset(&model, n * 1000 + 500);
    }
    for second in [32_500, 33_500, 34_500] {
        run(
            &mut words_,
            &model,
            Op::Locals,
            &[Some(at_second(second))],
            &[true],
            true,
        )
        .unwrap();
        today.offset(&model, second);
        assert_eq!(spans(&words_), today.spans, "{second}");
    }
}

/// The midnights of date_trunc are learned in the order of the rows, after
/// every local time, as the C code learned them: day 0's row first, whose
/// span is not kept, then day 1024's, whose span is; both midnights on one
/// slot, which holds day 1024's at the end.
#[test]
fn midnights_are_learned_in_the_order_of_the_rows() {
    let model = Model {
        initial: 0,
        transitions: vec![(EPOCH_SECONDS + 500 * SECS_PER_DAY, 3600)],
        failing: 0,
    };
    let noon = |day: i64| Some(day * USECS_PER_DAY + USECS_PER_DAY / 2);
    let mut words_ = vec![0_u64; CACHE_WORDS];
    run(
        &mut words_,
        &model,
        Op::Locals,
        &[noon(1024)],
        &[true],
        true,
    )
    .unwrap();
    let batch = [noon(0), noon(1024)];
    let run = run(
        &mut words_,
        &model,
        Op::Truncate(Unit::Day),
        &batch,
        &[true; 2],
        true,
    )
    .unwrap();
    for (row, value) in batch.iter().enumerate() {
        let want = model.reference(Op::Truncate(Unit::Day), value.unwrap());
        assert_eq!(run.results[row], Some(want), "row {row}");
    }
    // The slot of days 0 and 1024: its state, then its day.
    let slot = 3 + SPANS * 3;
    assert_eq!((words_[slot], words_[slot + 1]), (1, 1024));
}

/// A change of zone forgets the spans or the midnights apart, and the
/// spans' next victim stays, as the C code kept them.
#[test]
fn the_spans_and_the_midnights_reset_apart() {
    let model = steps(40);
    let mut words_ = vec![0_u64; CACHE_WORDS];
    let spans_of: Vec<Option<i64>> = (0..33).map(|n| Some(at_second(n * 1000 + 500))).collect();
    run(
        &mut words_,
        &model,
        Op::Locals,
        &spans_of,
        &[true; 33],
        true,
    )
    .unwrap();
    run(
        &mut words_,
        &model,
        Op::Midnights,
        &[Some(7)],
        &[true],
        true,
    )
    .unwrap();
    let victim = words_[2];
    assert_eq!(victim, 1);
    let midnights: Vec<u64> = words_[3 + SPANS * 3..].to_vec();
    Cache::new(&mut words_).unwrap().reset_spans();
    assert_eq!((words_[0], words_[1], words_[2]), (0, 0, victim));
    assert!(words_[3..3 + SPANS * 3].iter().all(|&word| word == 0));
    assert_eq!(words_[3 + SPANS * 3..], midnights[..]);
    let again = run(
        &mut words_,
        &model,
        Op::Midnights,
        &[Some(7)],
        &[true],
        true,
    )
    .unwrap();
    assert_eq!(again.asked, 0, "the midnights stay");
    run(
        &mut words_,
        &model,
        Op::Locals,
        &[Some(at_second(500))],
        &[true],
        true,
    )
    .unwrap();
    let spans_now = spans(&words_);
    Cache::new(&mut words_).unwrap().reset_midnights();
    assert_eq!(spans(&words_), spans_now, "the spans stay");
    assert!(words_[3 + SPANS * 3..].iter().all(|&word| word == 0));
}

/// A full cache whose next victim a later row reads: the C code evicts
/// that span and learns it again; the module answered the row in its first
/// call and keeps the cache without it. The answers are the same.
#[test]
fn a_full_cache_that_evicts_a_span_a_later_row_reads() {
    let model = steps(40);
    let mut words_ = vec![0_u64; CACHE_WORDS];
    let mut today = Today::new();
    // Spans 0 to 31 into both caches, slot by slot.
    let first: Vec<Option<i64>> = (0..32).map(|n| Some(at_second(n * 1000 + 500))).collect();
    run(&mut words_, &model, Op::Locals, &first, &[true; 32], true).unwrap();
    for n in 0..32 {
        today.offset(&model, n * 1000 + 500);
    }
    assert_eq!(spans(&words_), today.spans);
    // Span 32, then span 0, whose slot is the next victim.
    let batch = [Some(at_second(32_500)), Some(at_second(500))];
    let module = run(&mut words_, &model, Op::Locals, &batch, &[true; 2], true).unwrap();
    today.offset(&model, 32_500);
    today.offset(&model, 500);
    let expected: Vec<_> = [32_500, 500]
        .iter()
        .map(|&s| Some(Some(model.local(at_second(s)))))
        .collect();
    assert_eq!(module.results, expected);
    assert!(
        spans(&words_).iter().all(|span| span.0 != 500),
        "span 0 evicted"
    );
    assert!(
        today.spans.iter().any(|span| span.0 == 500),
        "the C code learned it again"
    );
}

/// Two days of one slot of the midnights, the second replacing the first;
/// a day whose midnight is out of the timestamps kept as none, and asked
/// once; a reset forgets everything.
#[test]
fn midnights_of_one_slot_and_a_missing_one() {
    let model = Model {
        initial: 9 * 3600,
        transitions: Vec::new(),
        failing: 0,
    };
    let day = 7300_i64;
    let dates = [Some(day), Some(day + MIDNIGHTS as i64), Some(day)];
    let mut words_ = vec![0_u64; CACHE_WORDS];
    let run1 = run(&mut words_, &model, Op::Midnights, &dates, &[true; 3], true).unwrap();
    assert_eq!(run1.asked, 2);
    // The day after the first date east of UTC: its midnight is before the
    // first timestamp, so the core takes it; asked once, then kept.
    let first = [Some(-i64::from(POSTGRES_EPOCH_JDATE))];
    let run2 = run(&mut words_, &model, Op::Midnights, &first, &[true], true).unwrap();
    assert_eq!(run2.results, vec![Some(None)]);
    assert_eq!(run2.asked, 1);
    let run3 = run(&mut words_, &model, Op::Midnights, &first, &[true], true).unwrap();
    assert_eq!((run3.asked, run3.calls), (0, 1));
    Cache::new(&mut words_).unwrap().reset();
    assert!(words_.iter().all(|&word| word == 0));
    let run4 = run(&mut words_, &model, Op::Midnights, &first, &[true], true).unwrap();
    assert_eq!(run4.asked, 1);
    // A day before the Julian day 0 is the core's, asked nothing, as the C
    // code leaves it.
    let before = [Some(-i64::from(POSTGRES_EPOCH_JDATE) - 1)];
    let run5 = run(&mut words_, &model, Op::Midnights, &before, &[true], true).unwrap();
    assert_eq!((run5.results.clone(), run5.asked), (vec![Some(None)], 0));
}

/// A span with no end, a second the library fails on, more spans in one
/// batch than the cache keeps: the results are the model's.
#[test]
fn spans_without_end_failing_seconds_and_more_spans_than_slots() {
    let model = Model {
        failing: 3,
        ..steps(100)
    };
    let values: Vec<Option<i64>> = (0..100)
        .map(|n| Some(at_second((n * 37 % 100) * 1000 + 999)))
        .chain([Some(at_second(200_000)), Some(at_second(3000))])
        .collect();
    let selected = vec![true; values.len()];
    let mut words_ = vec![0_u64; CACHE_WORDS];
    for op in [
        Op::Locals,
        Op::Truncate(Unit::Hour),
        Op::Truncate(Unit::Day),
        Op::Dates,
    ] {
        let run = run(&mut words_, &model, op, &values, &selected, true).unwrap();
        for (row, result) in run.results.iter().enumerate() {
            assert_eq!(
                *result,
                Some(model.reference(op, values[row].unwrap())),
                "{op:?} row {row}"
            );
        }
    }
}

/// A cache of another count of words, and an answer of a span that ends
/// at its second, are refused.
#[test]
fn a_wrong_cache_or_answer_is_refused() {
    assert_eq!(
        CACHE_WORDS,
        3 + 32 * 3 + 1024 * 3,
        "the header's TESS_ZONE_CACHE_WORDS"
    );
    let mut short = vec![0_u64; CACHE_WORDS - 1];
    assert!(Cache::new(&mut short).is_err());
    // A header no cache writes: more spans than slots, a last span or a
    // next victim past them.
    for (word, value) in [(0, 33_u64), (1, 32), (2, 32)] {
        let mut damaged = vec![0_u64; CACHE_WORDS];
        damaged[word] = value;
        assert!(Cache::new(&mut damaged).is_err(), "word {word} at {value}");
        damaged[word] = value - 1;
        assert!(
            Cache::new(&mut damaged).is_ok(),
            "word {word} at {}",
            value - 1
        );
    }
    let mut words_ = vec![0_u64; CACHE_WORDS];
    let mut cache = Cache::new(&mut words_).unwrap();
    let answers = [Answer::Span {
        second: 5,
        offset: 0,
        end: Some(5),
    }];
    let mut ready_words = [0_u64];
    let mut core_words = [0_u64];
    let mut unknown_words = [0_u64];
    let selection = [1_u64];
    let mut ready = RowMask::try_new(1, &mut ready_words).unwrap();
    let mut core = RowMask::try_new(1, &mut core_words).unwrap();
    let mut unknown = RowMask::try_new(1, &mut unknown_words).unwrap();
    let mut questions = [Question::Span(0)];
    let mut out = Outcome {
        ready: &mut ready,
        core: &mut core,
        unknown: &mut unknown,
        questions: &mut questions,
        asked: 0,
    };
    let mut locals = [MaybeUninit::new(0_i64)];
    let instants = Column(vec![Some(0_i64)]);
    let rows = RowMaskView::try_new(1, &selection).unwrap();
    assert!(zone::locals(&mut cache, &answers, &instants, rows, &mut locals, &mut out).is_err());
}
