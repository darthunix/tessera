//! The phases of a shared build: participants insert the inner side into
//! one table in shared memory, grow it once if it filled up, and probe it.
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
//! - [`ELECT`]: every participant arrives; one is elected;
//! - [`ALLOCATE`]: the elected one creates the table in a region sized by
//!   the planner's estimate and clears the shared Bloom filter;
//! - [`BUILD`]: every participant inserts its share of the inner side, and
//!   stages the rows a full table has no room for, reporting how many;
//! - [`GROW`]: when some rows were staged, the elected one copies the table
//!   into a region for every record and grows it;
//! - [`LINK`]: when some rows were staged, each participant adds its own;
//! - [`PROBE`]: every participant probes, then leaves; the last to leave
//!   frees the table.
//!
//! A participant that attaches late joins the phase the others are in:
//! during the build it inserts what is left of the inner side, which may
//! be nothing; from [`PROBE`] on it only probes; after the last one left,
//! it leaves at once.

use core::sync::atomic::AtomicU64;

use anyhow::{Result, ensure};

use super::region::order;

pub const ELECT: u32 = 0;
pub const ALLOCATE: u32 = 1;
pub const BUILD: u32 = 2;
pub const GROW: u32 = 3;
pub const LINK: u32 = 4;
pub const PROBE: u32 = 5;
pub const FREE: u32 = 6;

/// What a participant does next.
#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    /// Attach to the barrier and pass the phase it returns.
    Attach = 1,
    /// Arrive at the barrier and wait, and pass whether it was elected.
    ArriveAndWait = 2,
    /// Create the table and clear the filter, as the elected one.
    Allocate = 3,
    /// Insert this participant's share of the inner side, staging what
    /// does not fit, and report it.
    Build = 4,
    /// Copy the table into a region for every record and grow it, as the
    /// elected one.
    Grow = 5,
    /// Add this participant's staged rows.
    Link = 6,
    /// Probe; step again once done.
    Probe = 7,
    /// Arrive at the barrier and detach, and pass whether it was the last.
    ArriveAndDetach = 8,
    /// Detach from the barrier without arriving.
    Detach = 9,
    /// Free the table, as the last to leave; then nothing is left.
    Free = 10,
    /// Nothing is left to do.
    Done = 11,
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

/// The counters the participants of a build share: the rows staged, and
/// the payload words that hold a NULL somewhere.
pub(super) trait Counters {
    fn add_staged(&self, rows: u64);
    fn staged(&self) -> u64;
    fn add_null_columns(&self, bits: u64);
    fn null_columns(&self) -> u64;
}

/// The counters of a build in memory several participants map.
#[derive(Debug)]
pub struct SharedCounters<'a> {
    staged: &'a AtomicU64,
    null_columns: &'a AtomicU64,
}

/// The words of [`SharedCounters`].
pub const COUNTER_WORDS: usize = 2;

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
        // alignment of a `u64`.
        let all = unsafe { core::slice::from_raw_parts(words.cast::<AtomicU64>(), COUNTER_WORDS) };
        Ok(Self {
            staged: &all[0],
            null_columns: &all[1],
        })
    }

    /// Clear the counters, before any participant attaches.
    pub fn init(&self) {
        self.staged.store(0, order::RELAXED);
        self.null_columns.store(0, order::RELAXED);
    }

    /// Report what this participant's build staged and which payload
    /// words it saw a NULL in, before it arrives at the barrier.
    pub fn report(&self, staged: u64, null_columns: u64) {
        self.add_staged(staged);
        self.add_null_columns(null_columns);
    }

    /// The rows every participant staged, once the build is over.
    pub fn staged_rows(&self) -> u64 {
        self.staged()
    }

    /// The payload words with a NULL, once the build is over.
    pub fn nulls(&self) -> u64 {
        self.null_columns()
    }
}

// The barrier orders the reports before the reads: relaxed is enough.
impl Counters for SharedCounters<'_> {
    fn add_staged(&self, rows: u64) {
        self.staged.fetch_add(rows, order::RELAXED);
    }

    fn staged(&self) -> u64 {
        self.staged.load(order::RELAXED)
    }

    fn add_null_columns(&self, bits: u64) {
        self.null_columns.fetch_or(bits, order::RELAXED);
    }

    fn null_columns(&self) -> u64 {
        self.null_columns.load(order::RELAXED)
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

    pub(super) fn next<C: Counters + ?Sized>(
        &mut self,
        counters: &C,
        reply: u32,
    ) -> Result<Action> {
        let state = State::from_code(self.state)
            .ok_or_else(|| anyhow::anyhow!("a build participant in no known state"))?;
        Ok(match state {
            State::New => self.to(State::Attaching, Action::Attach),
            State::Attaching => {
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
    fn enter<C: Counters + ?Sized>(&mut self, counters: &C) -> Action {
        let elected = self.elected != 0;
        let staged = counters.staged() > 0;
        match self.phase {
            ALLOCATE if elected => self.to(State::Working, Action::Allocate),
            BUILD => self.to(State::Working, Action::Build),
            GROW if elected && staged => self.to(State::Working, Action::Grow),
            LINK if staged => self.to(State::Working, Action::Link),
            ELECT | ALLOCATE | GROW | LINK => self.to(State::Arriving, Action::ArriveAndWait),
            PROBE => self.to(State::Probing, Action::Probe),
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

    /// Counters for one participant alone.
    #[derive(Default)]
    struct Alone(core::cell::Cell<u64>);

    impl Counters for Alone {
        fn add_staged(&self, rows: u64) {
            self.0.set(self.0.get() + rows);
        }
        fn staged(&self) -> u64 {
            self.0.get()
        }
        fn add_null_columns(&self, _: u64) {}
        fn null_columns(&self) -> u64 {
            0
        }
    }

    /// The actions of a participant alone at the barrier, which elects it
    /// every time, until it is done.
    fn alone(staged: u64, attach_at: u32) -> Vec<Action> {
        let counters = Alone::default();
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
                    counters.add_staged(staged);
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
    fn a_participant_alone_skips_growth_when_nothing_was_staged() {
        use Action::*;
        assert_eq!(
            alone(0, ELECT),
            [
                Attach,
                ArriveAndWait,
                Allocate,
                ArriveAndWait,
                Build,
                ArriveAndWait,
                ArriveAndWait,
                ArriveAndWait,
                Probe,
                ArriveAndDetach,
                Free
            ]
        );
    }

    #[test]
    fn a_participant_alone_grows_and_links_what_it_staged() {
        use Action::*;
        assert_eq!(
            alone(3, ELECT),
            [
                Attach,
                ArriveAndWait,
                Allocate,
                ArriveAndWait,
                Build,
                ArriveAndWait,
                Grow,
                ArriveAndWait,
                Link,
                ArriveAndWait,
                Probe,
                ArriveAndDetach,
                Free
            ]
        );
    }

    #[test]
    fn a_late_participant_probes_or_leaves() {
        use Action::*;
        assert_eq!(alone(0, PROBE), [Attach, Probe, ArriveAndDetach, Free]);
        assert_eq!(alone(0, FREE), [Attach, Detach, Done]);
    }

    #[test]
    fn an_unknown_state_is_an_error() {
        let mut participant = Participant {
            phase: 0,
            state: 99,
            elected: 0,
        };
        assert!(participant.next(&Alone::default(), 0).is_err());
    }
}
