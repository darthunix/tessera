//! The machine of a build by several processes.
//!
//! A shared build is one hash table that several processes, its
//! participants, fill and probe together. Each participant appends its
//! share of the inner side to chunks of its own; the table may pass the
//! budget of every participant's memory while they append, and then it
//! splits into the partitions of a level ([`crate::plan`]), whose largest
//! go to disk while the build goes on. The phases of the build are those
//! of [`crate::phases`]: append, flush, size, link, write the outer side,
//! probe. This machine steps them and adds what a participant decides
//! about the disk:
//!
//! - **While it appends.** After each batch of a table that split, the
//!   host reports it ([`Event::Batch`]). A partition another participant
//!   sent to disk goes to disk here too: the machine names each one this
//!   participant still keeps in memory ([`Action::Demote`]). Then, while
//!   the table's chunks take more than their budget, the largest
//!   partition goes, by the rule of [`crate::shared_spill::Spill::evict`]
//!   over the words every participant shares ([`Action::Send`]).
//! - **At the flush.** A table that never split has nothing to write,
//!   and the participant goes on at once. One that split writes its
//!   chunks of each partition on disk ([`Action::Write`]) and gives those
//!   of the others to the table in memory ([`Action::Keep`]).
//! - **At the outer side.** Only a table that split writes its outer rows
//!   to the partitions' files ([`Action::Outer`]); one that never split
//!   goes on at once.
//! - **At the probe.** The probe knows whether the table split
//!   ([`Action::Probe`]).
//!
//! ```text
//!   Attach ──> Build ──> Flush ──> Size ──> Link ──> Outer ──> Probe ──> Leave
//!               │  ^      │  ^                                   │
//!         Batch v  │ Done  v  │ Done                             v
//!             Demote, Send  Write, Keep                       Free, Finished
//! ```
//!
//! The barriers are the host's: an action [`Action::Attach`],
//! [`Action::ArriveAndWait`], [`Action::ArriveAndDetach`] or
//! [`Action::Detach`] asks the host to do that at its barrier and to
//! report what it returned. An event that the action before it does not
//! take is a wrong call ([`Rule::JoinEvent`]), and the machine stays as
//! it was. The machine allocates nothing; the words it shares with the
//! other participants come in a [`Host`] at each step, and its own words
//! the host keeps for it.

use core::sync::atomic::AtomicU64;

use crate::error::ensure;
use crate::phases::{
    self, COUNTER_WORDS, CounterWords, Counters, Participant, SharedCounters, StopWord,
};
use crate::shared_spill::{Memory, SharedSpill, Spill, Weights, Words};
use crate::{Error, Result, Rule};

/// What every participant of a build shares: the weights of the rule that
/// sends the table's partitions to disk, as [`Weights`] names them.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Params {
    /// The share of the budget the chunks must pass for a partition to go,
    pub start: f64,
    /// and the share they must pass for each one after it in a check.
    pub target: f64,
    /// The weight of a partition's bytes in memory once it went to disk.
    pub spilled: f64,
    /// The most partitions one check sends, 0 for any.
    pub per_check: u32,
    /// 0.
    pub spare: u32,
}

/// What the host of a participant reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// The host starts the participant, or the action given is done.
    Done,
    /// The barrier's attach returned `phase`.
    Attached { phase: u32 },
    /// The barrier's arrive-and-wait returned; `elected` when it chose this
    /// participant.
    Arrived { elected: bool },
    /// The barrier's arrive-and-detach returned; `last` when this
    /// participant was the last attached.
    Left { last: bool },
    /// A batch of this participant's build went into a table that split,
    /// or the participant caught up with one: the partitions others sent to
    /// disk go there here too, and with `evict`, in the build only, the
    /// largest go while the table's chunks pass their budget; a catch-up
    /// at the flush, the outer side or the probe sends none, since the
    /// chunks no longer grow.
    Batch { evict: bool },
    /// The partition of the last [`Action::Send`] went to disk from here;
    /// `freed` when this participant's bytes of it in memory went down.
    Sent { freed: bool },
}

/// What a participant does next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Nothing more: the step's event is done, the work it came in goes on.
    Done,
    /// Attach to the barrier, and report the phase ([`Event::Attached`]).
    Attach,
    /// Arrive at the barrier and wait ([`Event::Arrived`]).
    ArriveAndWait,
    /// Arrive at the barrier and detach ([`Event::Left`]).
    ArriveAndDetach,
    /// Detach from the barrier without arriving, then [`Event::Done`].
    Detach,
    /// Append this participant's share of the inner side, reporting each
    /// batch of a table that split; then [`Event::Done`].
    Build,
    /// The table split: catch up with its level, then [`Event::Done`];
    /// the partitions follow.
    Flush,
    /// Write this participant's chunks of the partition to disk, then
    /// [`Event::Done`].
    Write(u32),
    /// Give this participant's chunks of the partition to the table in
    /// memory, then [`Event::Done`].
    Keep(u32),
    /// Every partition is written or kept: finish the files, then
    /// [`Event::Done`].
    FlushEnd,
    /// Make the index, as the elected participant, then [`Event::Done`].
    Size,
    /// Link this participant's chunks into the index, then
    /// [`Event::Done`].
    Link,
    /// The table split: write this participant's share of the outer side
    /// to the partitions' files, then [`Event::Done`].
    Outer,
    /// Probe, `split` when the table has partitions; then [`Event::Done`].
    Probe { split: bool },
    /// Free the table, as the last to leave; then nothing is left.
    Free,
    /// Nothing is left to do.
    Finished,
    /// The partition went to disk from another participant: write what
    /// this participant keeps of it in memory, then [`Event::Done`].
    Demote(u32),
    /// Send the partition to disk, then [`Event::Sent`].
    Send(u32),
}

/// The regions a step reads: the build's counters and the table's spill
/// words, which every participant shares, and one byte for each partition
/// of a table that split, not 0 when this participant keeps the partition
/// in memory; empty before the split.
#[derive(Debug)]
pub struct Host<'a> {
    pub counters: &'a [AtomicU64; COUNTER_WORDS],
    pub spill: &'a [AtomicU64],
    pub resident: &'a [u8],
}

// The action the machine gave last, by which it knows the event to take.
const NONE: u32 = 0;
const ATTACH: u32 = 1;
const ARRIVE: u32 = 2;
const LEAVE: u32 = 3;
const DETACH: u32 = 4;
const BUILD: u32 = 5;
const FLUSH: u32 = 6;
const WRITE: u32 = 7;
const FLUSH_END: u32 = 8;
const SIZE: u32 = 9;
const LINK: u32 = 10;
const OUTER: u32 = 11;
const PROBE: u32 = 12;
const FINISHED: u32 = 13;
const DEMOTE: u32 = 14;
const SEND: u32 = 15;

// Where a batch's check of the spill stands.
const SETTLED: u32 = 0;
const DEMOTING: u32 = 1;
const SENDING: u32 = 2;

/// The machine of one participant of a shared build.
///
/// It is plain data of a fixed layout, [`BUILD_WORDS`] words of 8 bytes,
/// which the participant keeps in its own memory. Any bytes may be given
/// to [`Build::step`]: those that are no machine are a wrong call
/// ([`Rule::JoinState`]).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Build {
    participant: Participant,
    /// The action given last, and the work a check of the spill came in.
    given: u32,
    work: u32,
    /// The check of the spill: its stage, and whether it sends after it
    /// demotes; the next partition to look at, of a check or of the flush;
    /// the partitions this check sent.
    stage: u32,
    evict: u32,
    cursor: u32,
    evicted: u32,
    /// 0: the words are whole, with no byte of padding a host would hold
    /// unwritten.
    spare: u32,
    /// The evictions of the table this participant has seen.
    seen: u64,
    params: Params,
}

// Its size is part of the mirror a host keeps: words of 8 bytes.
const _: () = assert!(size_of::<Build>() == 8 * BUILD_WORDS);

/// Words of 8 bytes a [`Build`] takes.
pub const BUILD_WORDS: usize = 10;

impl Build {
    /// The machine of a participant that has not attached yet.
    ///
    /// A wrong call: a weight below zero or not finite, or a spare word
    /// set ([`Rule::JoinParams`]).
    pub fn new(params: &Params) -> Result<Self> {
        ensure!(counted(params), Error::wrong_call(Rule::JoinParams));
        Ok(Self {
            participant: Participant::new(),
            given: NONE,
            work: NONE,
            stage: SETTLED,
            evict: 0,
            cursor: 0,
            evicted: 0,
            spare: 0,
            seen: 0,
            params: *params,
        })
    }

    /// The phase of the build the participant is in.
    pub fn phase(&self) -> u32 {
        self.participant.phase()
    }

    /// Take the host's event and give the next action.
    ///
    /// A wrong call: memory that is no machine ([`Rule::JoinState`]); an
    /// event the last action does not take ([`Rule::JoinEvent`]); spill
    /// words of no size [`crate::shared_spill::words_for`] gives
    /// ([`Rule::SpillWords`]); flags of partitions kept in memory for
    /// another number of partitions than the table's
    /// ([`Rule::ResidentFlags`]); and those of the phases
    /// ([`Participant::step`]). The machine is then as it was.
    pub fn step(&mut self, event: Event, host: &Host<'_>) -> Result<Action> {
        let counters = SharedCounters::new(host.counters);
        let spill = SharedSpill::new(host.spill)?;
        self.next(event, &counters, &spill, host.resident)
    }

    /// Before the participant leaves while it probes, its share of the
    /// outer side not done: as [`Participant::stop`].
    pub fn stop(&self, word: &StopWord<'_>) -> bool {
        self.participant.stop(word)
    }

    pub(super) fn next<C: CounterWords, S: Words>(
        &mut self,
        event: Event,
        counters: &Counters<C>,
        spill: &Spill<S>,
        resident: &[u8],
    ) -> Result<Action> {
        let mut next = *self;
        let action = next.take(event, counters, spill, resident)?;
        // A check of the spill over, the work it came in goes on.
        next.given = if action == Action::Done {
            next.work
        } else {
            code(action)
        };
        *self = next;
        Ok(action)
    }

    fn take<C: CounterWords, S: Words>(
        &mut self,
        event: Event,
        counters: &Counters<C>,
        spill: &Spill<S>,
        resident: &[u8],
    ) -> Result<Action> {
        self.check()?;
        let phases = |build: &mut Self, reply: u32| build.participant.next(counters, reply);
        match (self.given, event) {
            (NONE | DETACH, Event::Done) | (ATTACH, Event::Attached { .. }) => {
                let reply = match event {
                    Event::Attached { phase } => phase,
                    _ => 0,
                };
                let action = phases(self, reply)?;
                self.enter(action, counters, spill)
            }
            (ARRIVE, Event::Arrived { elected }) => {
                let action = phases(self, u32::from(elected))?;
                self.enter(action, counters, spill)
            }
            (LEAVE, Event::Left { last }) => {
                let action = phases(self, u32::from(last))?;
                self.enter(action, counters, spill)
            }
            (BUILD | FLUSH | OUTER | PROBE, Event::Batch { evict }) => {
                ensure!(
                    spill.partitions() > 0,
                    Error::wrong_call(Rule::JoinEvent)
                        .with(self.given)
                        .and(event_code(&event))
                );
                self.work = self.given;
                self.evict = u32::from(evict && self.given == BUILD);
                self.stage = DEMOTING;
                self.cursor = 0;
                let evictions = spill.evictions();
                if evictions == self.seen {
                    // Nothing went to disk since the last check.
                    self.cursor = spill.partitions();
                }
                self.seen = evictions;
                self.demote(spill, resident)
            }
            (DEMOTE, Event::Done) => self.demote(spill, resident),
            (SEND, Event::Sent { freed }) => {
                if !freed {
                    return Ok(self.settle());
                }
                self.evicted += 1;
                self.send(spill)
            }
            (FLUSH, Event::Done) => {
                self.cursor = 0;
                self.flush(spill)
            }
            (WRITE, Event::Done) => self.flush(spill),
            (BUILD | FLUSH_END | SIZE | LINK | OUTER | PROBE, Event::Done) => {
                let action = phases(self, 0)?;
                self.enter(action, counters, spill)
            }
            (given, event) => Err(Error::wrong_call(Rule::JoinEvent)
                .with(given)
                .and(event_code(&event))),
        }
    }

    /// The words, checked: an action the machine gives, a stage it has,
    /// and parameters it was made of.
    fn check(&self) -> Result<()> {
        ensure!(
            self.given <= SEND
                && self.stage <= SENDING
                && self.evict <= 1
                && self.spare == 0
                && counted(&self.params),
            Error::wrong_call(Rule::JoinState).with(self.given)
        );
        Ok(())
    }

    /// The first action of what the phases ask for: the work of the flush
    /// and of the outer side only for a table that split.
    fn enter<C: CounterWords, S: Words>(
        &mut self,
        action: phases::Action,
        counters: &Counters<C>,
        spill: &Spill<S>,
    ) -> Result<Action> {
        let split = spill.partitions() > 0;
        Ok(match action {
            phases::Action::Attach => Action::Attach,
            phases::Action::ArriveAndWait => Action::ArriveAndWait,
            phases::Action::ArriveAndDetach => Action::ArriveAndDetach,
            phases::Action::Detach => Action::Detach,
            phases::Action::Free => Action::Free,
            phases::Action::Done => Action::Finished,
            phases::Action::Build => Action::Build,
            phases::Action::Size => Action::Size,
            phases::Action::Link => Action::Link,
            phases::Action::Probe => Action::Probe { split },
            phases::Action::Flush if split => Action::Flush,
            phases::Action::Outer if split => Action::Outer,
            // Nothing to write: the phase's work is done at once.
            phases::Action::Flush | phases::Action::Outer => {
                let action = self.participant.next(counters, 0)?;
                return self.enter(action, counters, spill);
            }
            phases::Action::Allocate | phases::Action::Load => {
                return Err(Error::wrong_call(Rule::JoinState).with(self.given));
            }
        })
    }

    /// The next partition on disk that this participant keeps in memory;
    /// then, for a batch of the build, the sends.
    fn demote<S: Words>(&mut self, spill: &Spill<S>, resident: &[u8]) -> Result<Action> {
        let partitions = spill.partitions();
        ensure!(
            resident.len() == partitions as usize,
            Error::wrong_call(Rule::ResidentFlags)
                .with(resident.len())
                .and(partitions)
        );
        while self.cursor < partitions {
            let partition = self.cursor;
            self.cursor += 1;
            if resident[partition as usize] != 0 && spill.on_disk(partition)? {
                return Ok(Action::Demote(partition));
            }
        }
        if self.evict == 0 {
            return Ok(self.settle());
        }
        self.stage = SENDING;
        self.evicted = 0;
        self.send(spill)
    }

    /// The largest partition, while the table's chunks pass their budget.
    fn send<S: Words>(&mut self, spill: &Spill<S>) -> Result<Action> {
        let weights = Weights {
            start: self.params.start,
            target: self.params.target,
            spilled: self.params.spilled,
            reserve: 0,
            resident: 0.0,
            per_check: self.params.per_check,
        };
        Ok(
            match spill.evict(&weights, Memory::Counted, None, self.evicted)? {
                Some(partition) => Action::Send(partition),
                None => self.settle(),
            },
        )
    }

    /// The check of the spill is over: the work it came in goes on.
    fn settle(&mut self) -> Action {
        self.stage = SETTLED;
        self.evict = 0;
        Action::Done
    }

    /// The next partition of the flush, written or kept; then the end.
    fn flush<S: Words>(&mut self, spill: &Spill<S>) -> Result<Action> {
        if self.cursor < spill.partitions() {
            let partition = self.cursor;
            self.cursor += 1;
            return Ok(if spill.on_disk(partition)? {
                Action::Write(partition)
            } else {
                Action::Keep(partition)
            });
        }
        Ok(Action::FlushEnd)
    }
}

/// Whether `params` are those of a build: weights not below zero and
/// finite, and the spare word 0.
fn counted(params: &Params) -> bool {
    [params.start, params.target, params.spilled]
        .iter()
        .all(|weight| weight.is_finite() && *weight >= 0.0)
        && params.spare == 0
}

/// The code by which the machine remembers the action it gave.
fn code(action: Action) -> u32 {
    match action {
        Action::Attach => ATTACH,
        Action::ArriveAndWait => ARRIVE,
        Action::ArriveAndDetach => LEAVE,
        Action::Detach => DETACH,
        Action::Build => BUILD,
        Action::Flush => FLUSH,
        Action::Write(_) | Action::Keep(_) => WRITE,
        Action::FlushEnd => FLUSH_END,
        Action::Size => SIZE,
        Action::Link => LINK,
        Action::Outer => OUTER,
        Action::Probe { .. } => PROBE,
        Action::Free | Action::Finished => FINISHED,
        Action::Demote(_) => DEMOTE,
        Action::Send(_) => SEND,
        Action::Done => NONE,
    }
}

/// The number of an event in a [`Rule::JoinEvent`].
fn event_code(event: &Event) -> u32 {
    match event {
        Event::Done => 1,
        Event::Attached { .. } => 2,
        Event::Arrived { .. } => 3,
        Event::Left { .. } => 4,
        Event::Batch { .. } => 5,
        Event::Sent { .. } => 6,
    }
}

#[cfg(test)]
mod tests;
