//! The phases of a shared build: participants append the inner side to
//! chunks of their own, one of them sizes the index exactly, and all link
//! their chunks into it and probe it. Records never move and the table
//! never grows.
//!
//! A participant is a state machine that never waits itself: each step
//! returns an action, and the caller performs it and passes the result of
//! a barrier operation to the next step. The barrier is PostgreSQL's
//! `Barrier`, whose `BarrierAttach` returns the phase and whose
//! `BarrierArriveAndWait` elects one participant for the next phase; its
//! waits may raise an error, so they stay on the C side of the node, and
//! this module decides only what to do next. The loom model in `loom.rs`
//! runs these steps with a model of that barrier.
//!
//! The phases, in the barrier's own numbering:
//!
//! - [`BUILD`]: every participant appends its share of the inner side to
//!   chunks of its own, numbered by the shared counter, and reports how
//!   many records it appended;
//! - [`FLUSH`]: every participant writes its chunks of the partitions that
//!   went to disk, when the table spilled, and finishes its files;
//! - [`SIZE`]: the elected one creates the index for exactly the records
//!   appended, those in memory when the table spilled, and gathers the
//!   chunks' directory;
//! - [`LINK`]: every participant links its own chunks into the index;
//! - [`OUTER`]: when the table spilled, every participant writes its share
//!   of the outer side to the partitions' files, before any row goes out,
//!   so that no participant waits once it returns rows;
//! - [`PROBE`]: every participant probes, then leaves; the last to leave
//!   frees the table.
//!
//! A participant that attaches late joins the phase the others are in:
//! during the build it appends what is left of the inner side, which may
//! be nothing; in [`FLUSH`] it has nothing to write, from [`SIZE`] on no
//! chunk to link; in [`OUTER`] it writes what is left of the outer side;
//! after the last one left, it leaves at once.
//!
//! A RIGHT or FULL join marks the records its pairs match, and the last
//! participant to leave the table returns the records without a mark. A
//! participant that leaves while it probes, before its share of the outer
//! side is done, as one whose plan wants no more rows does, says so in a
//! [`StopWord`] first: the last one then returns no record without a
//! mark, since the pairs of the rows left unprobed were never marked, as
//! PostgreSQL's parallel hash join skips its unmatched rows then.

use core::sync::atomic::AtomicU64;

use anyhow::{Result, ensure};

use super::region::order;

pub const BUILD: u32 = 0;
pub const FLUSH: u32 = 1;
pub const SIZE: u32 = 2;
pub const LINK: u32 = 3;
pub const OUTER: u32 = 4;
pub const PROBE: u32 = 5;
pub const FREE: u32 = 6;

/// The phases of a round over one partition a shared table spilled, in its
/// own barrier's numbering, as the core's batches of a parallel hash join
/// have them: the participants that attach elect one, which makes the
/// partition's index; all load its inner rows from the files and link
/// them; all probe it with its outer rows and leave without waiting,
/// since they return rows; the last to leave frees it. One that attaches
/// once the round is freed has nothing to do there.
pub const ROUND_ELECT: u32 = 0;
pub const ROUND_ALLOCATE: u32 = 1;
pub const ROUND_LOAD: u32 = 2;
pub const ROUND_PROBE: u32 = 3;
pub const ROUND_FREE: u32 = 4;

/// What a participant does next.
#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    /// Attach to the barrier and pass the phase it returns.
    Attach = 1,
    /// Arrive at the barrier and wait, and pass whether it was elected.
    ArriveAndWait = 2,
    /// Append this participant's share of the inner side to chunks of its
    /// own, and report it.
    Build = 3,
    /// Create the index for every record appended and the chunks'
    /// directory, as the elected one.
    Size = 4,
    /// Link this participant's own chunks into the index.
    Link = 5,
    /// Probe; step again once done.
    Probe = 6,
    /// Arrive at the barrier and detach, and pass whether it was the last.
    ArriveAndDetach = 7,
    /// Detach from the barrier without arriving.
    Detach = 8,
    /// Free the table, as the last to leave; then nothing is left.
    Free = 9,
    /// Nothing is left to do.
    Done = 10,
    /// Write this participant's chunks of the partitions on disk, when the
    /// table spilled, and finish its files.
    Flush = 11,
    /// Write this participant's share of the outer side to the partitions'
    /// files, when the table spilled.
    Outer = 12,
    /// Make the partition's index, as the elected one of a round.
    Allocate = 13,
    /// Load inner files of the partition, taken one at a time, and link
    /// their records.
    Load = 14,
}

/// Where a participant stands between steps.
#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum State {
    New = 0,
    Attaching = 1,
    Arriving = 2,
    Working = 3,
    Probing = 4,
    Leaving = 5,
    Detaching = 6,
    Finished = 7,
}

impl State {
    fn from_code(code: u32) -> Option<Self> {
        Some(match code {
            0 => Self::New,
            1 => Self::Attaching,
            2 => Self::Arriving,
            3 => Self::Working,
            4 => Self::Probing,
            5 => Self::Leaving,
            6 => Self::Detaching,
            7 => Self::Finished,
            _ => return None,
        })
    }
}

/// One participant of a shared build, kept by the participant itself.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Participant {
    phase: u32,
    state: u32,
    elected: u32,
}

/// The counters of a build over their words: the records appended, the
/// payload words that hold a NULL somewhere, the chunks numbered so far,
/// and the records linked whose keys the table held already.
#[derive(Debug)]
pub struct Counters<W> {
    words: W,
}

/// The counters of a build in memory several participants map.
pub type SharedCounters<'a> = Counters<&'a [AtomicU64; COUNTER_WORDS]>;

/// The words of [`Counters`].
pub const COUNTER_WORDS: usize = 4;

// The words of the counters, by index.
const RECORDS: usize = 0;
const NULL_COLUMNS: usize = 1;
const CHUNKS: usize = 2;
const DUPLICATES: usize = 3;

pub(super) use sealed::CounterWords;

mod sealed {
    /// The words a build's counters lie in, by index: the atomic words of
    /// memory several processes map, or of the loom model.
    pub trait CounterWords {
        fn load(&self, index: usize) -> u64;
        fn store(&self, index: usize, value: u64);
        fn fetch_or(&self, index: usize, bits: u64) -> u64;
        /// Replace `current` by `new`: `Ok` with the value replaced, `Err`
        /// with the value found.
        fn compare_exchange(&self, index: usize, current: u64, new: u64) -> Result<u64, u64>;
    }
}

// The barrier orders every report before the totals are read, and a chunk
// number is unique by the swap alone: relaxed is enough.
impl CounterWords for &[AtomicU64; COUNTER_WORDS] {
    fn load(&self, index: usize) -> u64 {
        self[index].load(order::RELAXED)
    }

    fn store(&self, index: usize, value: u64) {
        self[index].store(value, order::RELAXED);
    }

    fn fetch_or(&self, index: usize, bits: u64) -> u64 {
        self[index].fetch_or(bits, order::RELAXED)
    }

    fn compare_exchange(&self, index: usize, current: u64, new: u64) -> Result<u64, u64> {
        self[index].compare_exchange(current, new, order::RELAXED, order::RELAXED)
    }
}

/// Whether a participant of a table left while it probed: set before it
/// leaves, read by the last participant to leave, after the barrier that
/// orders every leaving before the last one's.
pub(super) trait Stop {
    fn stop(&self);
    fn stopped(&self) -> bool;
}

/// [`Stop`] in a word several participants map, of a build or of a
/// round: 0 until a participant stops.
#[derive(Debug)]
pub struct StopWord<'a>(&'a AtomicU64);

impl<'a> StopWord<'a> {
    /// Attach to the word at `word`, cleared before any participant
    /// attached to the table.
    ///
    /// # Safety
    ///
    /// `word` is aligned to 8 and valid for reads and writes for `'a`, and
    /// accessed only through stop words, here or in other processes.
    pub unsafe fn attach(word: *mut u64) -> Result<Self> {
        ensure!(
            !word.is_null() && word.addr().is_multiple_of(8),
            "a stop word must be aligned to 8 bytes"
        );
        // SAFETY: the caller's contract; an `AtomicU64` has the size and
        // alignment of a `u64`.
        Ok(Self(unsafe { &*word.cast::<AtomicU64>() }))
    }

    /// Whether a participant stopped, once this one has left the table.
    pub fn stopped(&self) -> bool {
        Stop::stopped(self)
    }
}

// The barrier orders the setting before the read: relaxed is enough.
impl Stop for StopWord<'_> {
    fn stop(&self) {
        self.0.store(1, order::RELAXED);
    }

    fn stopped(&self) -> bool {
        self.0.load(order::RELAXED) != 0
    }
}

impl<'a> SharedCounters<'a> {
    /// Attach to the [`COUNTER_WORDS`] words at `words`.
    ///
    /// # Safety
    ///
    /// `words` is aligned to 8 and valid for reads and writes of
    /// [`COUNTER_WORDS`] words for `'a`, which are accessed only through
    /// shared counters, here or in other processes mapping them.
    pub unsafe fn attach(words: *mut u64) -> Result<Self> {
        ensure!(
            !words.is_null() && words.addr().is_multiple_of(8),
            "build counters must be aligned to 8 bytes"
        );
        // SAFETY: the caller's contract; an `AtomicU64` has the size and
        // alignment of a `u64`, and the array of the words their layout.
        let words = unsafe { &*words.cast::<[AtomicU64; COUNTER_WORDS]>() };
        Ok(Self { words })
    }
}

impl<W: CounterWords> Counters<W> {
    /// The counters over `words`, which [`Counters::init`] clears.
    #[cfg(all(test, loom))]
    pub(super) fn over(words: W) -> Self {
        Self { words }
    }

    /// Clear the counters, before any participant attaches.
    pub fn init(&self) {
        for index in [RECORDS, NULL_COLUMNS, CHUNKS, DUPLICATES] {
            self.words.store(index, 0);
        }
    }

    /// Report the records this participant appended and the payload words
    /// it saw a NULL in, before it arrives at the barrier; refused, the
    /// counters unchanged, when the records would pass their word.
    pub fn report(&self, records: u64, null_columns: u64) -> Result<()> {
        self.add_checked(RECORDS, records, "records")?;
        self.words.fetch_or(NULL_COLUMNS, null_columns);
        Ok(())
    }

    /// The records every participant appended, once the build is over.
    pub fn total_records(&self) -> u64 {
        self.words.load(RECORDS)
    }

    /// The payload words with a NULL, once the build is over.
    pub fn nulls(&self) -> u64 {
        self.words.load(NULL_COLUMNS)
    }

    /// The number of a new chunk: every participant's chunks are numbered
    /// from 0 on, in the order they were taken.
    pub fn take_chunk(&self) -> Result<u64> {
        self.add_checked(CHUNKS, 1, "chunks")
    }

    /// The chunks numbered so far, once the build is over.
    pub fn total_chunks(&self) -> u64 {
        self.words.load(CHUNKS)
    }

    /// Add the duplicates this participant's links found, before it
    /// arrives at the barrier after linking.
    pub fn add_duplicates(&self, duplicates: u64) -> Result<()> {
        self.add_checked(DUPLICATES, duplicates, "duplicates")
            .map(|_| ())
    }

    /// The duplicates every participant found, once linking is over.
    pub fn total_duplicates(&self) -> u64 {
        self.words.load(DUPLICATES)
    }

    /// Add `delta` to a counter by a compare-and-swap: the count before,
    /// or an error when the sum would pass the word, the counter then
    /// unchanged.
    fn add_checked(&self, index: usize, delta: u64, what: &str) -> Result<u64> {
        let mut count = self.words.load(index);
        loop {
            let Some(sum) = count.checked_add(delta) else {
                anyhow::bail!("the {what} of a build, {count}, cannot take {delta} more");
            };
            match self.words.compare_exchange(index, count, sum) {
                Ok(before) => return Ok(before),
                Err(found) => count = found,
            }
        }
    }
}

impl Participant {
    /// A participant that has not attached yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// The phase the participant is in.
    pub fn phase(&self) -> u32 {
        self.phase
    }

    /// The next action, after the previous one is done: `reply` is the
    /// phase [`Action::Attach`] returned, whether [`Action::ArriveAndWait`]
    /// elected this participant or [`Action::ArriveAndDetach`] found it
    /// the last (1 or 0), and is ignored after any other action.
    pub fn step(&mut self, counters: &SharedCounters<'_>, reply: u32) -> Result<Action> {
        self.next(counters, reply)
    }

    pub(super) fn next<W: CounterWords>(
        &mut self,
        counters: &Counters<W>,
        reply: u32,
    ) -> Result<Action> {
        let state = State::from_code(self.state)
            .ok_or_else(|| anyhow::anyhow!("a build participant in no known state"))?;
        Ok(match state {
            State::New => self.to(State::Attaching, Action::Attach),
            State::Attaching => {
                ensure!(
                    reply <= FREE,
                    "a build participant attached at phase {reply}, past the last"
                );
                self.phase = reply;
                self.elected = 0;
                self.enter(counters)
            }
            State::Arriving => {
                self.phase += 1;
                self.elected = u32::from(reply != 0);
                self.enter(counters)
            }
            State::Working => self.to(State::Arriving, Action::ArriveAndWait),
            State::Probing => self.to(State::Leaving, Action::ArriveAndDetach),
            State::Leaving => {
                let last = reply != 0;
                self.to(
                    State::Finished,
                    if last { Action::Free } else { Action::Done },
                )
            }
            State::Detaching | State::Finished => self.to(State::Finished, Action::Done),
        })
    }

    /// The first action of the phase just entered.
    fn enter<W: CounterWords>(&mut self, counters: &Counters<W>) -> Action {
        let elected = self.elected != 0;
        match self.phase {
            BUILD => self.to(State::Working, Action::Build),
            FLUSH => self.to(State::Working, Action::Flush),
            SIZE if elected => self.to(State::Working, Action::Size),
            LINK if counters.total_records() > 0 => self.to(State::Working, Action::Link),
            SIZE | LINK => self.to(State::Arriving, Action::ArriveAndWait),
            OUTER => self.to(State::Working, Action::Outer),
            PROBE => self.to(State::Probing, Action::Probe),
            _ => self.to(State::Detaching, Action::Detach),
        }
    }

    /// Before a participant leaves the table while it probes, its share of
    /// the outer side not done: `word` is marked, and true returned. In
    /// any other phase nothing is marked: the participant has matched no
    /// pair yet, or has probed every row of its share. The caller then
    /// steps on to leave, as for the build or the round the word is of.
    pub fn stop(&self, word: &StopWord<'_>) -> bool {
        self.stop_in(word)
    }

    pub(super) fn stop_in<S: Stop + ?Sized>(&self, word: &S) -> bool {
        let probing = self.state == State::Probing as u32;
        if probing {
            word.stop();
        }
        probing
    }

    /// The next action of a round over a partition, as [`Self::step`] for
    /// a build.
    pub fn round_step(&mut self, reply: u32) -> Result<Action> {
        let state = State::from_code(self.state)
            .ok_or_else(|| anyhow::anyhow!("a round participant in no known state"))?;
        Ok(match state {
            State::New => self.to(State::Attaching, Action::Attach),
            State::Attaching => {
                ensure!(
                    reply <= ROUND_FREE,
                    "a round participant attached at phase {reply}, past the last"
                );
                self.phase = reply;
                self.elected = 0;
                self.enter_round()
            }
            State::Arriving => {
                self.phase += 1;
                self.elected = u32::from(reply != 0);
                self.enter_round()
            }
            State::Working => self.to(State::Arriving, Action::ArriveAndWait),
            State::Probing => self.to(State::Leaving, Action::ArriveAndDetach),
            State::Leaving => {
                let last = reply != 0;
                self.to(
                    State::Finished,
                    if last { Action::Free } else { Action::Done },
                )
            }
            State::Detaching | State::Finished => self.to(State::Finished, Action::Done),
        })
    }

    /// The first action of the round's phase just entered.
    fn enter_round(&mut self) -> Action {
        match self.phase {
            ROUND_ELECT => self.to(State::Arriving, Action::ArriveAndWait),
            ROUND_ALLOCATE if self.elected != 0 => self.to(State::Working, Action::Allocate),
            ROUND_ALLOCATE => self.to(State::Arriving, Action::ArriveAndWait),
            ROUND_LOAD => self.to(State::Working, Action::Load),
            ROUND_PROBE => self.to(State::Probing, Action::Probe),
            _ => self.to(State::Detaching, Action::Detach),
        }
    }

    fn to(&mut self, state: State, action: Action) -> Action {
        self.state = state as u32;
        action
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real counters over a test's own words.
    fn attach_counters(words: &mut [u64; COUNTER_WORDS]) -> SharedCounters<'_> {
        // SAFETY: the array is aligned to 8 and used only through these
        // counters while they live.
        unsafe { SharedCounters::attach(words.as_mut_ptr()) }.unwrap()
    }

    /// The actions of a participant alone at the barrier, which elects it
    /// every time, until it is done.
    fn alone(records: u64, attach_at: u32) -> Vec<Action> {
        alone_after(0, records, attach_at)
    }

    /// The actions of [`alone`] for a participant that attaches once
    /// others, gone since, appended `before` records.
    fn alone_after(before: u64, records: u64, attach_at: u32) -> Vec<Action> {
        let mut words = [before, 0, 0, 0];
        let counters = attach_counters(&mut words);
        let mut participant = Participant::new();
        let mut actions = Vec::new();
        let mut reply = 0;
        let mut phase = attach_at;
        loop {
            let action = participant.next(&counters, reply).unwrap();
            actions.push(action);
            reply = match action {
                Action::Attach => phase,
                Action::ArriveAndWait => {
                    phase += 1;
                    1
                }
                Action::Build => {
                    counters.report(records, 0).unwrap();
                    0
                }
                Action::ArriveAndDetach => 1,
                Action::Free | Action::Done => break,
                _ => 0,
            };
        }
        actions
    }

    #[test]
    fn a_participant_alone_builds_flushes_sizes_links_and_probes() {
        use Action::*;
        assert_eq!(
            alone(3, BUILD),
            [
                Attach,
                Build,
                ArriveAndWait,
                Flush,
                ArriveAndWait,
                Size,
                ArriveAndWait,
                Link,
                ArriveAndWait,
                Outer,
                ArriveAndWait,
                Probe,
                ArriveAndDetach,
                Free
            ]
        );
    }

    /// A stop word in this process.
    #[derive(Default)]
    struct Word(core::cell::Cell<bool>);

    impl Stop for Word {
        fn stop(&self) {
            self.0.set(true);
        }
        fn stopped(&self) -> bool {
            self.0.get()
        }
    }

    /// A participant marks the stop word only while it probes: before the
    /// probe it has matched no pair, and once it left it probed its share.
    #[test]
    fn a_participant_stops_only_while_it_probes() {
        let mut words = [0; COUNTER_WORDS];
        let counters = attach_counters(&mut words);
        let mut participant = Participant::new();
        let mut reply = 0;
        let mut probed = false;
        loop {
            let word = Word::default();
            let marked = participant.stop_in(&word);
            assert_eq!((marked, word.stopped()), (probed, probed));
            let action = participant.next(&counters, reply).unwrap();
            probed = action == Action::Probe;
            reply = match action {
                Action::Attach => PROBE,
                Action::ArriveAndDetach => 1,
                Action::Free | Action::Done => break,
                _ => 0,
            };
        }
        let word = Word::default();
        assert!(
            !participant.stop_in(&word) && !word.stopped(),
            "once it left"
        );
    }

    #[test]
    fn nothing_appended_is_nothing_to_link() {
        use Action::*;
        assert_eq!(
            alone(0, BUILD),
            [
                Attach,
                Build,
                ArriveAndWait,
                Flush,
                ArriveAndWait,
                Size,
                ArriveAndWait,
                ArriveAndWait,
                Outer,
                ArriveAndWait,
                Probe,
                ArriveAndDetach,
                Free
            ]
        );
    }

    /// A participant that attaches late joins the phase the others are in:
    /// it writes what is left at FLUSH and OUTER, waits at SIZE, where it
    /// was not elected, links its own chunks, none, at LINK, probes at
    /// PROBE and leaves at once at FREE.
    #[test]
    fn a_late_participant_joins_the_phase_the_others_are_in() {
        use Action::*;
        assert_eq!(
            alone_after(3, 0, FLUSH),
            [
                Attach,
                Flush,
                ArriveAndWait,
                Size,
                ArriveAndWait,
                Link,
                ArriveAndWait,
                Outer,
                ArriveAndWait,
                Probe,
                ArriveAndDetach,
                Free
            ]
        );
        assert_eq!(
            alone_after(3, 0, SIZE),
            [
                Attach,
                ArriveAndWait,
                Link,
                ArriveAndWait,
                Outer,
                ArriveAndWait,
                Probe,
                ArriveAndDetach,
                Free
            ]
        );
        assert_eq!(
            alone_after(3, 0, LINK),
            [
                Attach,
                Link,
                ArriveAndWait,
                Outer,
                ArriveAndWait,
                Probe,
                ArriveAndDetach,
                Free
            ]
        );
        assert_eq!(
            alone(0, OUTER),
            [Attach, Outer, ArriveAndWait, Probe, ArriveAndDetach, Free]
        );
        assert_eq!(alone(0, PROBE), [Attach, Probe, ArriveAndDetach, Free]);
        assert_eq!(alone(0, FREE), [Attach, Detach, Done]);
    }

    /// Three participants number chunks, report and add duplicates at
    /// once over the real counters: no number is given twice, the records
    /// and the duplicates are their sums, and the NULL bits their OR.
    #[test]
    fn participants_count_a_build_at_once() {
        let mut words = [u64::MAX; COUNTER_WORDS];
        // SAFETY: the array is aligned to 8 and used only through these
        // counters.
        let counters = unsafe { SharedCounters::attach(words.as_mut_ptr()) }.unwrap();
        counters.init();
        let numbers: Vec<u64> = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..3_u64)
                .map(|participant| {
                    let counters = &counters;
                    scope.spawn(move || {
                        let numbers: Vec<u64> =
                            (0..50).map(|_| counters.take_chunk().unwrap()).collect();
                        counters.report(10 + participant, 1 << participant).unwrap();
                        counters.add_duplicates(participant).unwrap();
                        numbers
                    })
                })
                .collect();
            threads
                .into_iter()
                .flat_map(|thread| thread.join().unwrap())
                .collect()
        });
        let mut numbers = numbers;
        numbers.sort_unstable();
        assert_eq!(numbers, (0..150).collect::<Vec<_>>(), "each number once");
        assert_eq!(counters.total_records(), 33);
        assert_eq!(counters.nulls(), 0b111);
        assert_eq!(counters.total_chunks(), 150);
        assert_eq!(counters.total_duplicates(), 3);
    }

    /// The actions of a participant alone in a round attached at a phase.
    fn round_alone(attach_at: u32) -> Vec<Action> {
        let mut participant = Participant::new();
        let mut actions = Vec::new();
        let mut reply = 0;
        let mut phase = attach_at;
        loop {
            let action = participant.round_step(reply).unwrap();
            actions.push(action);
            reply = match action {
                Action::Attach => phase,
                Action::ArriveAndWait => {
                    phase += 1;
                    1
                }
                Action::ArriveAndDetach => 1,
                Action::Free | Action::Done => break,
                _ => 0,
            };
        }
        actions
    }

    #[test]
    fn a_round_elects_allocates_loads_probes_and_frees() {
        use Action::*;
        assert_eq!(
            round_alone(ROUND_ELECT),
            [
                Attach,
                ArriveAndWait,
                Allocate,
                ArriveAndWait,
                Load,
                ArriveAndWait,
                Probe,
                ArriveAndDetach,
                Free
            ]
        );
        assert_eq!(
            round_alone(ROUND_ALLOCATE),
            [
                Attach,
                ArriveAndWait,
                Load,
                ArriveAndWait,
                Probe,
                ArriveAndDetach,
                Free
            ],
            "not elected, it waits"
        );
        assert_eq!(
            round_alone(ROUND_LOAD),
            [Attach, Load, ArriveAndWait, Probe, ArriveAndDetach, Free]
        );
        assert_eq!(
            round_alone(ROUND_PROBE),
            [Attach, Probe, ArriveAndDetach, Free]
        );
        assert_eq!(round_alone(ROUND_FREE), [Attach, Detach, Done]);
    }

    /// An attach that answers a phase past the last, of a build or of a
    /// round, comes from no barrier of a sound node: the step refuses it.
    #[test]
    fn an_attach_past_the_last_phase_is_an_error() {
        let mut words = [0; COUNTER_WORDS];
        let counters = attach_counters(&mut words);
        let mut build = Participant::new();
        assert_eq!(build.next(&counters, 0).unwrap(), Action::Attach);
        assert!(build.next(&counters, FREE + 1).is_err());
        assert!(build.next(&counters, u32::MAX).is_err());
        let mut round = Participant::new();
        assert_eq!(round.round_step(0).unwrap(), Action::Attach);
        assert!(round.round_step(ROUND_FREE + 1).is_err());
        assert!(round.round_step(u32::MAX).is_err());
    }

    /// A count that would pass its word is refused, and the counter keeps
    /// its value: records, chunks and duplicates alike.
    #[test]
    fn counts_past_their_words_are_refused() {
        let mut words = [0_u64; COUNTER_WORDS];
        // SAFETY: the array is aligned to 8 and used only through these
        // counters.
        let counters = unsafe { SharedCounters::attach(words.as_mut_ptr()) }.unwrap();
        counters.init();
        counters.report(u64::MAX, 0b01).unwrap();
        assert!(counters.report(1, 0b10).is_err());
        assert_eq!(
            (counters.total_records(), counters.nulls()),
            (u64::MAX, 0b01),
            "the counters unchanged"
        );
        counters.add_duplicates(u64::MAX).unwrap();
        assert!(counters.add_duplicates(1).is_err());
        assert_eq!(counters.total_duplicates(), u64::MAX);
        counters.words.store(CHUNKS, u64::MAX - 1);
        assert_eq!(counters.take_chunk().unwrap(), u64::MAX - 1);
        assert!(counters.take_chunk().is_err());
        assert_eq!(counters.total_chunks(), u64::MAX);
    }

    #[test]
    fn an_unknown_state_is_an_error() {
        let mut participant = Participant {
            phase: 0,
            state: 99,
            elected: 0,
        };
        let mut words = [0; COUNTER_WORDS];
        assert!(participant.next(&attach_counters(&mut words), 0).is_err());
    }
}
