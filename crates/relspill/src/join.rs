//! The machine of a hash join's spill.
//!
//! A hash join builds a table from its inner rows and probes it with its
//! outer rows. When the table outgrows its memory limit, the join
//! spills: the rows go into the partitions of a level ([`crate::plan`]),
//! some partitions stay in memory and the others go to disk, a chunk at
//! a time, with the outer rows of their partitions. The machine is the
//! protocol of that spill. The host, the join, holds it in memory of its
//! own and steps it: it reports what it saw as an [`Event`], and the
//! machine answers with the [`Action`] the host does next.
//!
//! ```text
//!   Fits ──Inner──> Planned ──Made──> Building ──BuildEnd──> Probing ──OuterEnd──> Joining
//!     │ (fits: Done)                   │    ^                    │
//!     │                                │    │ Sent               │ Full (outer)
//!     │                         Grown, │    │                    │
//!     │                         Full   v    │
//!     │                               Sending
//! ```
//!
//! - **Fits.** The table is in memory. Before each batch of inner rows the
//!   host reports [`Event::Inner`] with the bytes the table would take
//!   with it. Past the limit the machine plans the first level and
//!   answers [`Action::Start`]; otherwise [`Action::Done`].
//! - **Planned.** The host makes the level, its spill words of
//!   [`Plan::words`](crate::plan::Plan) among them, and reports
//!   [`Event::Made`]: the machine clears the words for the level's
//!   partitions. The host then moves the table into the partitions, and
//!   goes on with the inner rows.
//! - **Building.** A partition whose chunk is full is reported as
//!   [`Event::Full`]: one in memory, or without a chunk in memory, gets
//!   a new one ([`Action::AddChunk`]); one on disk writes what it holds
//!   ([`Action::Flush`]). After each batch the host reports
//!   [`Event::Grown`] with the memory it measures; while that passes the
//!   limit, the rule of [`Spill::evict`](crate::shared_spill::Spill::evict) picks a partition to send to
//!   disk, the host sends it and reports [`Event::Sent`], until the rule
//!   picks none or a send frees nothing. [`Event::BuildEnd`] ends the
//!   inner rows: the partitions in memory that hold too few of them go
//!   too, and the machine sets where each partition's outer rows go
//!   ([`Route`]).
//! - **Probing.** The outer rows of the partitions on disk are written by
//!   their partitions, a chunk at a time ([`Event::Full`] of the outer
//!   side), until [`Event::OuterEnd`].
//! - **Joining.** The partitions on disk are joined, one at a time; the
//!   host decides how.
//!
//! An event that the machine's phase does not take is a wrong call
//! ([`Rule::JoinEvent`]), and the machine stays as it was. The machine
//! decides by the numbers the host gives: its memory, its bytes and its
//! rows. It does nothing for each row: the host puts the rows into their
//! partitions, and where a decision is made for each row, the machine
//! makes it once for each partition, as the routes of the outer rows.
//!
//! A level below, which splits a partition of a level too large, is a
//! machine of its own ([`Join::below`]), which starts planned.
//!
//! The machine holds no memory: it is plain data, which the host keeps
//! where it likes, and every region it reads comes in a [`Host`] at each
//! step. It allocates nothing.

use core::cell::Cell;

use crate::columns;
use crate::error::ensure;
use crate::plan::{self, Level, Plan, Shape};
use crate::shared_spill::{LocalSpill, Memory, Records, Weights};
use crate::{Error, Result, Rule};

/// The least outer rows a chunk of them holds, however wide they are.
pub const LEAST_OUTER_ROWS: usize = 4;

/// What every level of one join's spill shares, given once.
///
/// It is plain data of a fixed layout, so that a host in another
/// language can make it; every bit pattern is a value, which
/// [`Join::new`] checks.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Params {
    /// The memory the join may take, in bytes, which the rule that sends
    /// partitions to disk weighs the host's memory against.
    pub limit: u64,
    /// The shape of every level's inner side ([`crate::plan`]).
    pub shape: Shape,
    /// The words an outer row stores in a chunk of columns
    /// ([`crate::columns`]), its NULL bits aside.
    pub outer_words: u64,
    /// 1 when an outer row keeps values by reference, 0 when it keeps none:
    /// a partition on disk then holds a chunk of them beside its tail.
    pub outer_values: u32,
    /// Zero.
    pub spare: u32,
    /// The bytes of the buffer a file of outer rows keeps.
    pub page: u64,
    /// The weights of the rule that sends partitions to disk, as
    /// [`Weights`] names them: `start`, `target`, `spilled` and
    /// `resident`;
    pub start: f64,
    pub target: f64,
    pub spilled: f64,
    pub resident: f64,
    /// and the share of the outer tail that each partition on disk keeps
    /// in reserve: its chunk of outer rows, its chunk of values when they
    /// keep values, and the page of its file.
    pub tail: f64,
}

/// A step of the host: what it saw.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Event {
    /// A batch of inner rows is about to go into the table in memory,
    /// which takes `held` bytes in `rows` rows of the `estimate` the host
    /// expects; with the batch, its index and the chunks it may add, it
    /// would take `needed`.
    Inner {
        held: u64,
        rows: u64,
        estimate: f64,
        needed: u64,
    },
    /// The level of the last [`Action::Start`] is made, with its spill
    /// words.
    Made,
    /// A partition of one side has no room for its next row, or its
    /// values passed a chunk; `tail` when it holds a chunk of rows in
    /// memory.
    Full {
        side: Side,
        partition: u32,
        tail: bool,
    },
    /// Rows went into the partitions: the host's memory now.
    Grown { memory: u64 },
    /// The partition of the last [`Action::Demote`] or [`Action::Evict`]
    /// went to disk: whether that freed memory, and the memory now.
    Sent { freed: bool, memory: u64 },
    /// The inner rows are read.
    BuildEnd,
    /// The outer rows are read.
    OuterEnd,
}

/// The side of a join a partition's rows are of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    /// The inner rows, which make the table.
    Inner,
    /// The outer rows, which probe it.
    Outer,
}

/// What the host does next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Nothing more: the step's event is done.
    Done,
    /// The table spills: make the level of the plan, then report
    /// [`Event::Made`].
    Start(Start),
    /// Give the partition of the event a new chunk.
    AddChunk,
    /// Write what the partition of the event holds in memory, its values
    /// and its chunk, whose memory takes its next rows.
    Flush,
    /// Send this partition, in memory, to disk: write its values and its
    /// chunks, and keep the last one, emptied, as its tail; then report
    /// [`Event::Sent`].
    Demote(u32),
    /// This partition, on disk, writes what it holds in memory and frees
    /// it; then report [`Event::Sent`].
    Evict(u32),
}

/// A level to make.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Start {
    /// Its plan: the partitions and their chunks of inner rows.
    pub plan: Plan,
    /// The bytes of its chunks of outer rows: a chunk of the plan, or as
    /// many as [`LEAST_OUTER_ROWS`] rows take, if more.
    pub outer_len: usize,
}

/// Where the outer rows of a partition go, once the inner rows are read.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// In memory: the rows probe the table now.
    Probe = 0,
    /// On disk: the rows are written by their partition, and joined with
    /// it later.
    Write = 1,
    /// On disk without an inner row: the rows have no pair, and are
    /// answered now.
    Answer = 2,
}

/// The regions of the host a step reads.
#[derive(Debug)]
pub struct Host<'a> {
    /// The level's spill words, [`Plan::words`](crate::plan::Plan) of
    /// them, whose bytes the host counts by partition
    /// ([`LocalSpill::add_bytes`]); none while the table fits.
    pub words: &'a [Cell<u64>],
    /// The inner rows of each partition, and of the level.
    pub rows: &'a [u64],
    pub total: u64,
    /// A byte for each partition, which [`Event::BuildEnd`] sets to a
    /// [`Route`].
    pub routes: &'a mut [u8],
}

/// The phases, as the machine keeps them; 0 is none, as of memory never
/// made into a machine.
const FITS: u32 = 1;
const PLANNED: u32 = 2;
const BUILDING: u32 = 3;
const SENDING: u32 = 4;
const ENDING: u32 = 5;
const PROBING: u32 = 6;
const JOINING: u32 = 7;

/// The machine of one level of a join's spill.
///
/// It is plain data of a fixed layout, [`JOIN_WORDS`] words of 8 bytes,
/// so that a host in another language keeps it in memory of its own; a
/// step checks what it reads, and memory that is no machine is a wrong
/// call ([`Rule::JoinState`]).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Join {
    params: Params,
    /// The level's: its limit, the length of its chunks of inner and of
    /// outer rows, the reserve of a partition on disk, and its words.
    limit: u64,
    chunk_len: u64,
    outer_len: u64,
    reserve: u64,
    words: u64,
    phase: u32,
    level: u32,
    partitions: u32,
    shift: u32,
    below: u32,
    deeper: u32,
    /// The partitions the current check sent to disk.
    evicted: u32,
    spare: u32,
}

// Its size is part of the mirror a host keeps: words of 8 bytes.
const _: () = assert!(size_of::<Join>() == 8 * JOIN_WORDS);

/// Words of 8 bytes a [`Join`] takes.
pub const JOIN_WORDS: usize = 24;

impl Join {
    /// The machine of a join whose table fits so far.
    ///
    /// A wrong call: a limit of 0 or weights below zero or not finite
    /// ([`Rule::JoinParams`]); outer rows of more words than a chunk of
    /// columns stores ([`Rule::ColumnsWords`]); and a shape that no level
    /// can be planned by ([`plan::plan`]).
    pub fn new(params: &Params) -> Result<Self> {
        check(params)?;
        Ok(Self {
            params: *params,
            limit: params.limit,
            chunk_len: 0,
            outer_len: 0,
            reserve: 0,
            words: 0,
            phase: FITS,
            level: 0,
            partitions: 0,
            shift: 0,
            below: 0,
            deeper: 0,
            evicted: 0,
            spare: 0,
        })
    }

    /// The machine of the level `level` below the first, which splits a
    /// partition of `expected` bytes within `limit`, from the hash bit
    /// `shift`, and the level to make; it waits for [`Event::Made`].
    ///
    /// The wrong calls of [`Join::new`], a level 0, and those of
    /// [`plan::plan`].
    pub fn below(
        params: &Params,
        level: u32,
        expected: f64,
        limit: u64,
        shift: u32,
    ) -> Result<(Self, Start)> {
        ensure!(level > 0, Error::wrong_call(Rule::JoinLevel).with(level));
        let mut join = Self::new(params)?;
        let start = join.plan_level(level, expected, limit, shift)?;
        Ok((join, start))
    }

    /// The first level of a build that several processes share, whose
    /// `partitions` one of them planned: the level each of the others
    /// makes, with its chunks of outer rows. The shared build decides the
    /// rest of the level.
    ///
    /// The wrong calls of [`Join::new`] and those of [`plan::plan`] for
    /// given partitions.
    pub fn given(params: &Params, partitions: u32) -> Result<Start> {
        let mut join = Self::new(params)?;
        join.plan_given(0, 0.0, params.limit, 0, partitions)
    }

    /// The level: 0 for the first.
    pub fn level(&self) -> u32 {
        self.level
    }

    /// Take the host's event and give its next action.
    ///
    /// A wrong call: memory that is no machine ([`Rule::JoinState`]); an
    /// event its phase does not take ([`Rule::JoinEvent`]); a partition
    /// past the level's ([`Rule::Partition`]); spill words that are not
    /// the level's ([`Rule::SpillWords`]); counts of rows or routes for
    /// another number of partitions ([`Rule::RecordCounts`],
    /// [`Rule::JoinRoutes`]); and those of [`plan::plan`] for the level
    /// to start. The machine is then as it was, though a refused
    /// [`Event::Made`] may have cleared the words.
    pub fn step(&mut self, event: Event, host: &mut Host<'_>) -> Result<Action> {
        let phase = self.phase()?;
        match (phase, event) {
            (
                FITS,
                Event::Inner {
                    held,
                    rows,
                    estimate,
                    needed,
                },
            ) => {
                if needed <= self.limit {
                    return Ok(Action::Done);
                }
                // Not below zero, and not a number refused too.
                let counted = estimate >= 0.0;
                ensure!(counted, Error::wrong_call(Rule::Expected));
                let expected = plan::expected(held as f64, rows as f64, estimate, 1);
                let mut next = *self;
                let start = next.plan_level(0, expected, self.params.limit, 0)?;
                *self = next;
                Ok(Action::Start(start))
            }
            (PLANNED, Event::Made) => {
                ensure!(
                    host.words.len() as u64 == self.words,
                    Error::wrong_call(Rule::SpillWords)
                        .with(host.words.len())
                        .and(self.words)
                );
                let spill = LocalSpill::new(host.words)?;
                spill.init(0);
                spill.split(self.partitions)?;
                self.phase = BUILDING;
                Ok(Action::Done)
            }
            (
                BUILDING,
                Event::Full {
                    side: Side::Inner,
                    partition,
                    tail,
                },
            )
            | (
                PROBING,
                Event::Full {
                    side: Side::Outer,
                    partition,
                    tail,
                },
            ) => {
                let spill = self.spill(host)?;
                ensure!(
                    partition < self.partitions,
                    Error::wrong_call(Rule::Partition)
                        .with(partition)
                        .and(self.partitions)
                );
                // The outer rows written are those of partitions on disk.
                let on_disk = phase == PROBING || spill.on_disk(partition)?;
                Ok(if on_disk && tail {
                    Action::Flush
                } else {
                    Action::AddChunk
                })
            }
            (BUILDING, Event::Grown { memory }) => self.send(host, Some(memory), 0),
            (SENDING, Event::Sent { freed, memory }) => {
                if !freed {
                    self.phase = BUILDING;
                    return Ok(Action::Done);
                }
                self.send(host, Some(memory), self.evicted + 1)
            }
            (BUILDING, Event::BuildEnd) => self.send(host, None, 0),
            (ENDING, Event::Sent { freed, .. }) => {
                if !freed {
                    return self.route(host);
                }
                // The end weighs the rows alone: no share of the limit, so no count.
                self.send(host, None, 0)
            }
            (PROBING, Event::OuterEnd) => {
                self.phase = JOINING;
                Ok(Action::Done)
            }
            _ => Err(Error::wrong_call(Rule::JoinEvent)
                .with(phase)
                .and(code(&event))),
        }
    }

    /// The phase, checked.
    fn phase(&self) -> Result<u32> {
        ensure!(
            (FITS..=JOINING).contains(&self.phase),
            Error::wrong_call(Rule::JoinState).with(self.phase)
        );
        Ok(self.phase)
    }

    /// The level's spill words, checked as the level's.
    fn spill<'a>(&self, host: &Host<'a>) -> Result<LocalSpill<'a>> {
        let spill = LocalSpill::new(host.words)?;
        ensure!(
            host.words.len() as u64 == self.words && spill.partitions() == self.partitions,
            Error::wrong_call(Rule::SpillWords)
                .with(host.words.len())
                .and(self.words)
        );
        Ok(spill)
    }

    /// Plan level `level` of `expected` bytes within `limit` from hash bit
    /// `shift`: the machine waits for the level to be made.
    fn plan_level(&mut self, level: u32, expected: f64, limit: u64, shift: u32) -> Result<Start> {
        self.plan_given(level, expected, limit, shift, 0)
    }

    /// [`Join::plan_level`] of `partitions` another process planned, or 0
    /// to plan them.
    fn plan_given(
        &mut self,
        level: u32,
        expected: f64,
        limit: u64,
        shift: u32,
        partitions: u32,
    ) -> Result<Start> {
        let plan = plan::plan(
            &self.params.shape,
            &Level {
                expected,
                limit: usize::try_from(limit).unwrap_or(usize::MAX),
                shift,
                at_least: 0,
                partitions,
            },
        )?;
        let words = usize::try_from(self.params.outer_words).unwrap_or(usize::MAX);
        let outer_len = plan
            .chunk_len
            .max(columns::HEADER + 8 * LEAST_OUTER_ROWS * columns::lanes(words));
        let mut tail = outer_len as u64 + self.params.page;
        if self.params.outer_values != 0 {
            tail += outer_len as u64;
        }
        self.limit = limit;
        self.chunk_len = plan.chunk_len as u64;
        self.outer_len = outer_len as u64;
        self.reserve = (tail as f64 * self.params.tail) as u64;
        self.words = plan.words as u64;
        self.phase = PLANNED;
        self.level = level;
        self.partitions = plan.partitions;
        self.shift = plan.shift;
        self.below = plan.below;
        self.deeper = u32::from(plan.deeper);
        self.evicted = 0;
        Ok(Start { plan, outer_len })
    }

    /// The next partition the rule sends to disk, its `evicted`-th of the
    /// check: against the limit while building, by the share of the rows
    /// at the end (`memory` none); or, when it sends none, the end of the
    /// check.
    fn send(&mut self, host: &mut Host<'_>, memory: Option<u64>, evicted: u32) -> Result<Action> {
        let spill = self.spill(host)?;
        let weights = Weights {
            start: self.params.start,
            target: self.params.target,
            spilled: self.params.spilled,
            reserve: self.reserve,
            resident: self.params.resident,
            per_check: 0,
        };
        let measured = match memory {
            Some(bytes) => Memory::Measured {
                bytes,
                limit: self.params.limit,
            },
            None => Memory::Measured {
                bytes: 0,
                limit: u64::MAX,
            },
        };
        let records = Records {
            each: host.rows,
            total: host.total,
        };
        if memory.is_none() {
            check_routes(host, self.partitions)?;
        }
        match spill.pick(&weights, measured, Some(records), evicted)? {
            Some((partition, on_disk)) => {
                self.evicted = evicted;
                self.phase = if memory.is_some() { SENDING } else { ENDING };
                Ok(if on_disk {
                    Action::Evict(partition)
                } else {
                    Action::Demote(partition)
                })
            }
            None if memory.is_some() => {
                self.phase = BUILDING;
                Ok(Action::Done)
            }
            None => self.route(host),
        }
    }

    /// The end of the build: where each partition's outer rows go.
    fn route(&mut self, host: &mut Host<'_>) -> Result<Action> {
        let spill = self.spill(host)?;
        check_routes(host, self.partitions)?;
        ensure!(
            host.rows.len() == self.partitions as usize,
            Error::wrong_call(Rule::RecordCounts)
                .with(host.rows.len())
                .and(self.partitions)
        );
        for (partition, (route, rows)) in host.routes.iter_mut().zip(host.rows).enumerate() {
            *route = if !spill.on_disk(partition as u32)? {
                Route::Probe
            } else if *rows == 0 {
                Route::Answer
            } else {
                Route::Write
            } as u8;
        }
        self.phase = PROBING;
        Ok(Action::Done)
    }
}

/// Refuse routes for another number of partitions.
fn check_routes(host: &Host<'_>, partitions: u32) -> Result<()> {
    ensure!(
        host.routes.len() == partitions as usize,
        Error::wrong_call(Rule::JoinRoutes)
            .with(host.routes.len())
            .and(partitions)
    );
    Ok(())
}

/// Refuse parameters no level can be planned by.
fn check(params: &Params) -> Result<()> {
    let weights = [
        params.start,
        params.target,
        params.spilled,
        params.resident,
        params.tail,
    ];
    ensure!(
        params.limit > 0
            && params.spare == 0
            && weights
                .iter()
                .all(|weight| weight.is_finite() && *weight >= 0.0),
        Error::wrong_call(Rule::JoinParams)
    );
    ensure!(
        params.outer_words <= columns::MAX_WORDS as u64,
        Error::wrong_call(Rule::ColumnsWords)
            .with(params.outer_words)
            .and(columns::MAX_WORDS)
    );
    plan::plan(
        &params.shape,
        &Level::first(usize::try_from(params.limit).unwrap_or(usize::MAX)),
    )?;
    Ok(())
}

/// The number of an event in a [`Rule::JoinEvent`].
fn code(event: &Event) -> u32 {
    match event {
        Event::Inner { .. } => 1,
        Event::Made => 2,
        Event::Full { .. } => 3,
        Event::Grown { .. } => 4,
        Event::Sent { .. } => 5,
        Event::BuildEnd => 6,
        Event::OuterEnd => 7,
    }
}

#[cfg(test)]
mod tests;
