//! The machine of a grouping's spill.
//!
//! A grouping keeps one record for each group in a hash table, the
//! group's keys and the states of its aggregates. When the table outgrows
//! its memory limit, the grouping spills: the records go into the
//! partitions of a level ([`crate::plan`]), and while the table takes
//! more than the limit, the partition with the most bytes in memory goes
//! to disk whole. New rows of its groups make new records in memory,
//! which merge with those on disk once the input ends. The machine is the
//! protocol of that spill; the host, the grouping, holds it and steps it
//! with what it saw ([`Event`]), and does what the machine answers
//! ([`Action`]).
//!
//! ```text
//!   Fits ──Inner──> Planned ──Made──> Building ──InputEnd──> Flushing ──> Giving ──> Done
//!                                      │   ^                               │   ^
//!                               Grown  v   │ Sent                    Next  v   │ Weighed
//!                                     Sending                             Weighing
//! ```
//!
//! - **Fits.** After each batch the host reports the bytes its table
//!   takes ([`Event::Inner`]): past `fill` eighths of the limit, the
//!   machine plans the first level ([`Action::Start`]). A partial
//!   grouping, whose groups may go up unfinished, weighs its table first
//!   ([`Event::Partial`]): full, it sends it up ([`Action::Emit`]), or,
//!   when sending it up would fold too few rows, it spills instead
//!   ([`Action::Spill`]) and asks for the first level itself.
//! - **Planned.** The host makes the level and reports it
//!   ([`Event::Made`]), then moves its records into the partitions.
//! - **Building.** A partition whose chunk is full takes another
//!   ([`Event::Full`], [`Action::AddChunk`]): the first level keeps its
//!   records in memory until a partition goes to disk; a level below,
//!   which splits a partition read back, writes a full chunk and fills it
//!   again ([`Action::Flush`]). After a batch the host reports its memory
//!   ([`Event::Grown`]), and while that passes the limit the rule of
//!   [`Spill::evict`](crate::shared_spill::Spill::evict) picks a partition
//!   whose chunks the host writes and frees ([`Action::Write`]), until it
//!   picks none.
//! - **Flushing.** At the end of the input ([`Event::InputEnd`]) a
//!   partition with records on disk writes those in memory too, since it
//!   merges from disk anyway; the partitions wholly in memory stay.
//! - **Giving.** The host asks for the next partition to give out
//!   ([`Event::Next`]): first those wholly in memory, which merge with
//!   nothing ([`Action::Merge`]); then those with records on disk, which
//!   the machine weighs by the groups the host estimates they hold
//!   ([`Action::Weigh`], [`Event::Weighed`]): one too large for what the
//!   limit leaves splits into a level below ([`Action::Split`]), planned
//!   for what the spill's memory leaves of the limit, a quarter of it at
//!   least, as a join's level below is; the others merge. When none is left, the level is done
//!   ([`Action::LevelDone`]).
//!
//! An event that the machine's phase does not take is a wrong call
//! ([`Rule::JoinEvent`]), and the machine stays as it was. The machine
//! does nothing for each row, holds no memory and allocates nothing: it
//! is plain data the host keeps, and every region it reads comes in a
//! [`Host`] at each step.

use core::cell::Cell;

use crate::error::ensure;
use crate::plan::{self, Level, Plan, Shape};
use crate::shared_spill::{LocalSpill, Memory, Partition, SplitWeights, Weights, splits};
use crate::{Error, Result, Rule};

/// What every level of one grouping's spill shares, given once.
///
/// It is plain data of a fixed layout, so that a host in another
/// language can make it; every bit pattern is a value, which
/// [`Group::new`] checks.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Params {
    /// The memory the grouping may take, in bytes: every level's.
    pub limit: u64,
    /// The shape of every level ([`crate::plan`]).
    pub shape: Shape,
    /// The bytes a group takes merged: its record and its share of the
    /// index.
    pub merged: u64,
    /// The eighths of the limit past which the table is full: past
    /// `limit / 8 * fill` bytes, counted in whole eighths.
    pub fill: u64,
    /// The weights of the rule that sends partitions to disk, as
    /// [`Weights`] names them: `start`, `target` and `spilled`;
    pub start: f64,
    pub target: f64,
    pub spilled: f64,
    /// and the share of what the limit leaves a partition read back must
    /// pass to split ([`SplitWeights::room`]).
    pub split_room: f64,
}

/// A step of the host: what it saw.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Event {
    /// The table in memory takes `bytes` in `records` groups of the
    /// `estimate` the host expects; with `force`, the host spills
    /// whatever the bytes.
    Inner {
        bytes: u64,
        records: u64,
        estimate: f64,
        force: bool,
    },
    /// The level of the last [`Action::Start`] or of [`Group::below`] is
    /// made, with its spill words.
    Made,
    /// A partition's chunk has no room for its next record; `tail` when
    /// it holds one.
    Full { partition: u32, tail: bool },
    /// A partial grouping weighs its table before a batch: its groups
    /// take `memory` bytes in `records` groups, of `rows` rows read since
    /// the table started; `spills` when the groups can go to disk.
    Partial {
        memory: u64,
        records: u64,
        rows: u64,
        spills: bool,
    },
    /// Records went into the partitions: the host's memory now.
    Grown { memory: u64 },
    /// The partition of the last [`Action::Write`] went to disk, and the
    /// host's memory now.
    Sent { memory: u64 },
    /// The input is read.
    InputEnd,
    /// The host asks for the next partition to give out: the one it gave
    /// is done, or none was given yet.
    Next,
    /// The partition of the last [`Action::Weigh`] holds about `groups`
    /// groups; the other partitions in memory, of every level, take
    /// `others` bytes, and the spill `memory` bytes in all, which a level
    /// below plans against.
    Weighed {
        groups: u64,
        others: u64,
        memory: u64,
    },
}

/// What the host does next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Nothing more: the step's event is done.
    Done,
    /// The table spills: make the level of the plan, then report
    /// [`Event::Made`].
    Start(Plan),
    /// Give the partition of the event a new chunk.
    AddChunk,
    /// Write the full chunk of the partition of the event, and fill it
    /// again.
    Flush,
    /// Write every chunk this partition holds in memory and free them;
    /// then report [`Event::Sent`], or, at the end of the input,
    /// [`Event::Next`].
    Write(u32),
    /// Give this partition out: its records in memory make a table, which
    /// its records on disk merge into; then report [`Event::Next`].
    Merge(u32),
    /// Estimate the groups of this partition and report them
    /// ([`Event::Weighed`]).
    Weigh(u32),
    /// Split a partition into a level below, its machine by
    /// [`Group::below`], and give that level out before the next
    /// partition of this one.
    Split(Below),
    /// Every partition of the level is given out.
    LevelDone,
    /// The partial grouping's table is full: its groups go up as they
    /// are, and a new table starts.
    Emit,
    /// The partial grouping's table is full of groups of few rows each:
    /// it spills instead, and the host reports [`Event::Inner`] with
    /// `force`.
    Spill,
}

/// A level below to make, which splits a partition of the level above.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Below {
    /// The partition of the level above that it splits.
    pub partition: u32,
    /// Its level, one past the level above's.
    pub level: u32,
    /// The bytes it expects: the partition's on disk and in memory.
    pub expected: u64,
    /// The memory it may take: what the spill's memory leaves of the
    /// limit, a quarter of it at least.
    pub limit: u64,
    /// The first hash bit it takes, past those of the levels above.
    pub shift: u32,
}

/// The regions of the host a step reads.
#[derive(Debug)]
pub struct Host<'a> {
    /// The level's spill words, [`Plan::words`](crate::plan::Plan) of
    /// them, whose bytes the host counts by partition
    /// ([`LocalSpill::add_bytes`]); none while the table fits.
    pub words: &'a [Cell<u64>],
    /// The records in memory of each partition, and the bytes of its
    /// blocks on disk.
    pub records: &'a [u64],
    pub disk: &'a [u64],
}

/// The phases, as the machine keeps them; 0 is none, as of memory never
/// made into a machine.
const FITS: u32 = 1;
const PLANNED: u32 = 2;
const BUILDING: u32 = 3;
const SENDING: u32 = 4;
const FLUSHING: u32 = 5;
const GIVING: u32 = 6;
const WEIGHING: u32 = 7;
const DONE: u32 = 8;

/// The machine of one level of a grouping's spill.
///
/// It is plain data of a fixed layout, [`GROUP_WORDS`] words of 8 bytes,
/// so that a host in another language keeps it in memory of its own; a
/// step checks what it reads, and memory that is no machine is a wrong
/// call ([`Rule::JoinState`]).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Group {
    params: Params,
    /// The level's chunks and words.
    chunk_len: u64,
    words: u64,
    phase: u32,
    level: u32,
    partitions: u32,
    shift: u32,
    below: u32,
    /// A level of the shape fits below, in the bits left.
    deeper: u32,
    /// The partitions the current check sent to disk.
    evicted: u32,
    /// The next partition to flush or to give out, and the pass of the
    /// giving out: 0 for the partitions wholly in memory, 1 for the
    /// others.
    cursor: u32,
    pass: u32,
    /// 0: the last word is whole, with no byte of padding a host would
    /// hold unwritten.
    spare: u32,
}

// Its size is part of the mirror a host keeps: words of 8 bytes.
const _: () = assert!(size_of::<Group>() == 8 * GROUP_WORDS);

/// Words of 8 bytes a [`Group`] takes.
pub const GROUP_WORDS: usize = 20;

impl Group {
    /// The machine of a grouping whose table fits so far.
    ///
    /// A wrong call: a limit of 0, or a weight below zero or not finite
    /// ([`Rule::JoinParams`]); and a shape that no level can be planned by
    /// ([`plan::plan`]). Any `fill` is eighths: 0 makes any table with a
    /// group full, 8 or more one past the limit.
    pub fn new(params: &Params) -> Result<Self> {
        check(params)?;
        Ok(Self {
            params: *params,
            chunk_len: 0,
            words: 0,
            phase: FITS,
            level: 0,
            partitions: 0,
            shift: 0,
            below: 0,
            deeper: 0,
            evicted: 0,
            cursor: 0,
            pass: 0,
            spare: 0,
        })
    }

    /// The machine of a level below that [`Action::Split`] asked for, and
    /// its plan; it waits for [`Event::Made`].
    ///
    /// The wrong calls of [`Group::new`], a level 0, and those of
    /// [`plan::plan`].
    pub fn below(params: &Params, below: &Below) -> Result<(Self, Plan)> {
        ensure!(
            below.level > 0,
            Error::wrong_call(Rule::JoinLevel).with(below.level)
        );
        let mut group = Self::new(params)?;
        let plan =
            group.plan_level(below.level, below.expected as f64, below.limit, below.shift)?;
        Ok((group, plan))
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
    /// the level's ([`Rule::SpillWords`]); counts of records or bytes on
    /// disk for another number of partitions ([`Rule::RecordCounts`],
    /// [`Rule::JoinDisk`]); and those of [`plan::plan`] for the level to
    /// start. The machine is then as it was, though a refused
    /// [`Event::Made`] may have cleared the words.
    pub fn step(&mut self, event: Event, host: &Host<'_>) -> Result<Action> {
        let mut next = *self;
        let action = next.take(event, host)?;
        *self = next;
        Ok(action)
    }

    fn take(&mut self, event: Event, host: &Host<'_>) -> Result<Action> {
        let phase = self.phase()?;
        match (phase, event) {
            (
                FITS,
                Event::Inner {
                    bytes,
                    records,
                    estimate,
                    force,
                },
            ) => {
                if !force && !full(self.params.limit, self.params.fill, bytes) {
                    return Ok(Action::Done);
                }
                // Not below zero, and not a number refused too.
                let counted = estimate >= 0.0;
                ensure!(counted, Error::wrong_call(Rule::Expected));
                let expected = plan::expected(bytes as f64, records as f64, estimate, 1);
                Ok(Action::Start(self.plan_level(
                    0,
                    expected,
                    self.params.limit,
                    0,
                )?))
            }
            (
                FITS,
                Event::Partial {
                    memory,
                    records,
                    rows,
                    spills,
                },
            ) => Ok(
                // An empty table sent up would come back as it went, forever.
                if !full(self.params.limit, self.params.fill, memory) || records == 0 {
                    Action::Done
                } else if spills && records.saturating_mul(2) > rows {
                    // More groups than half the rows: sent up, it would
                    // fold nothing.
                    Action::Spill
                } else {
                    Action::Emit
                },
            ),
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
            (BUILDING, Event::Full { partition, tail }) => {
                self.spill(host)?;
                self.check_partition(partition)?;
                // A level below writes its partitions as they fill.
                Ok(if self.level > 0 && tail {
                    Action::Flush
                } else {
                    Action::AddChunk
                })
            }
            (BUILDING, Event::Grown { memory }) => self.send(host, memory, 0),
            (SENDING, Event::Sent { memory }) => self.send(host, memory, self.evicted + 1),
            (BUILDING, Event::InputEnd) => {
                self.counts(host)?;
                self.cursor = 0;
                self.phase = FLUSHING;
                self.flush(host)
            }
            (FLUSHING, Event::Next) => self.flush(host),
            (GIVING, Event::Next) => self.next(host),
            (
                WEIGHING,
                Event::Weighed {
                    groups,
                    others,
                    memory,
                },
            ) => self.weigh(host, groups, others, memory),
            _ => Err(Error::wrong_call(Rule::JoinEvent)
                .with(phase)
                .and(code(&event))),
        }
    }

    /// The phase, checked.
    fn phase(&self) -> Result<u32> {
        ensure!(
            (FITS..=DONE).contains(&self.phase),
            Error::wrong_call(Rule::JoinState).with(self.phase)
        );
        Ok(self.phase)
    }

    fn check_partition(&self, partition: u32) -> Result<()> {
        ensure!(
            partition < self.partitions,
            Error::wrong_call(Rule::Partition)
                .with(partition)
                .and(self.partitions)
        );
        Ok(())
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

    /// Refuse counts of records or bytes on disk for another number of
    /// partitions than the level's.
    fn counts(&self, host: &Host<'_>) -> Result<()> {
        let partitions = self.partitions as usize;
        for (what, len) in [
            (Rule::RecordCounts, host.records.len()),
            (Rule::JoinDisk, host.disk.len()),
        ] {
            ensure!(
                len == partitions,
                Error::wrong_call(what).with(len).and(partitions)
            );
        }
        Ok(())
    }

    /// Plan level `level` of `expected` bytes within `limit` from hash bit
    /// `shift`. The machine waits for the level to be made.
    fn plan_level(&mut self, level: u32, expected: f64, limit: u64, shift: u32) -> Result<Plan> {
        let plan = plan::plan(
            &self.params.shape,
            &Level {
                expected,
                limit: usize::try_from(limit).unwrap_or(usize::MAX),
                shift,
                at_least: 0,
                partitions: 0,
            },
        )?;
        self.chunk_len = plan.chunk_len as u64;
        self.words = plan.words as u64;
        self.phase = PLANNED;
        self.level = level;
        self.partitions = plan.partitions;
        self.shift = plan.shift;
        self.below = plan.below;
        self.deeper = u32::from(plan.deeper);
        self.evicted = 0;
        Ok(plan)
    }

    /// The next partition the rule sends to disk, its `evicted`-th of the
    /// check, or the end of the check.
    fn send(&mut self, host: &Host<'_>, memory: u64, evicted: u32) -> Result<Action> {
        let spill = self.spill(host)?;
        let weights = Weights {
            start: self.params.start,
            target: self.params.target,
            spilled: self.params.spilled,
            reserve: 0,
            resident: 0.0,
            per_check: 0,
        };
        let memory = Memory::Measured {
            bytes: memory,
            limit: self.params.limit,
        };
        match spill.pick(&weights, memory, None, evicted)? {
            Some((partition, _)) => {
                self.evicted = evicted;
                self.phase = SENDING;
                Ok(Action::Write(partition))
            }
            None => {
                self.phase = BUILDING;
                Ok(Action::Done)
            }
        }
    }

    /// The next partition with records on disk and chunks in memory,
    /// which it writes before the giving out; then the giving out.
    fn flush(&mut self, host: &Host<'_>) -> Result<Action> {
        let spill = self.spill(host)?;
        self.counts(host)?;
        while self.cursor < self.partitions {
            let partition = self.cursor;
            self.cursor += 1;
            if host.disk[partition as usize] > 0 && spill.held(partition)? > 0 {
                return Ok(Action::Write(partition));
            }
        }
        self.cursor = 0;
        self.pass = 0;
        self.phase = GIVING;
        Ok(Action::Done)
    }

    /// The next partition to give out: those wholly in memory, then
    /// those with records on disk, weighed first.
    fn next(&mut self, host: &Host<'_>) -> Result<Action> {
        self.counts(host)?;
        loop {
            while self.cursor < self.partitions {
                let partition = self.cursor;
                let index = partition as usize;
                self.cursor += 1;
                let on_disk = host.disk[index] > 0;
                if (host.records[index] == 0 && !on_disk) || (self.pass == 0 && on_disk) {
                    continue;
                }
                if !on_disk {
                    return Ok(Action::Merge(partition));
                }
                self.phase = WEIGHING;
                return Ok(Action::Weigh(partition));
            }
            if self.pass == 1 {
                self.phase = DONE;
                return Ok(Action::LevelDone);
            }
            self.pass = 1;
            self.cursor = 0;
        }
    }

    /// The partition weighed: split into a level below when one of the
    /// level's shape fits in the bits left and what its `groups` take
    /// merged passes `split_room` of what the limit leaves besides
    /// `others`, planned for what the spill's `memory` leaves of the
    /// limit; merged otherwise.
    fn weigh(&mut self, host: &Host<'_>, groups: u64, others: u64, memory: u64) -> Result<Action> {
        let spill = self.spill(host)?;
        self.counts(host)?;
        // The partition the cursor passed last, Weigh's.
        ensure!(
            (1..=self.partitions).contains(&self.cursor),
            Error::wrong_call(Rule::JoinState).with(self.phase)
        );
        let partition = self.cursor - 1;
        let index = partition as usize;
        // A record and its index a group, and a block read back.
        let size = groups
            .saturating_mul(self.params.merged)
            .saturating_add(self.chunk_len);
        let split = self.deeper != 0
            && splits(
                &SplitWeights {
                    room: self.params.split_room,
                    key: 0.0,
                },
                &Partition {
                    size,
                    used: others,
                    limit: self.params.limit,
                    rows: 0,
                    level_rows: 0,
                    bits: self.below,
                },
            )?;
        let action = if split {
            Action::Split(Below {
                partition,
                level: self
                    .level
                    .checked_add(1)
                    .ok_or(Error::wrong_call(Rule::JoinState).with(self.phase))?,
                expected: host.disk[index].saturating_add(spill.held(partition)?),
                limit: plan::below_limit(self.params.limit, memory),
                shift: self.below,
            })
        } else {
            Action::Merge(partition)
        };
        self.phase = GIVING;
        Ok(action)
    }
}

/// Refuse parameters no level can be planned by.
fn check(params: &Params) -> Result<()> {
    let weights = [
        params.start,
        params.target,
        params.spilled,
        params.split_room,
    ];
    ensure!(
        params.limit > 0
            && weights
                .iter()
                .all(|weight| weight.is_finite() && *weight >= 0.0),
        Error::wrong_call(Rule::JoinParams)
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
        Event::InputEnd => 6,
        Event::Next => 7,
        Event::Weighed { .. } => 8,
        Event::Partial { .. } => 9,
    }
}

/// Whether `bytes` pass `fill` eighths of `limit`.
fn full(limit: u64, fill: u64, bytes: u64) -> bool {
    bytes > (limit / 8).saturating_mul(fill)
}

/// What every level of a grouping's rows shares, given once: the rows of
/// the groups a table of aggregates whose states cannot merge lacks, once
/// it takes no new group.
///
/// It is plain data of a fixed layout; every bit pattern is a value,
/// which [`Rows::new`] checks.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RowsParams {
    /// The memory the grouping may take, in bytes.
    pub limit: u64,
    /// The shape of a level of rows ([`crate::plan`]): its fixed
    /// partitions and its chunks of rows.
    pub shape: Shape,
    /// The eighths of the limit past which a table takes no new group,
    /// as [`Params::fill`],
    pub fill: u64,
    /// once it has this many groups at least: fewer would freeze every
    /// table read back again, level after level.
    pub least: u64,
}

/// A step of the host of a grouping's rows: what it saw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowsEvent {
    /// After a batch, the table's groups take `memory` bytes in `records`
    /// groups.
    Grown { memory: u64, records: u64 },
    /// The rows of the level the last [`RowsAction::Freeze`] planned are
    /// all written.
    Written,
    /// The host asks for the level's next partition to read.
    Next,
}

/// What the host of a grouping's rows does next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowsAction {
    /// Nothing more: the step's event is done.
    Done,
    /// The table takes no new group: the rows of the groups it lacks go
    /// to the partitions of a level of `plan`, numbered `level`, which the
    /// host makes.
    Freeze { plan: Plan, level: u32 },
    /// Read this partition's rows into a table of their own, whose machine
    /// is [`Rows::read`] of it.
    Read(Table),
    /// Every partition of the level with rows is read.
    LevelDone,
}

/// A table of a partition of rows read back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Table {
    /// The partition read.
    pub partition: u32,
    /// The level a freeze of it writes, one past the partition's.
    pub level: u32,
    /// The first hash bit that level takes, past the partition's level's.
    pub shift: u32,
    /// No level fits in the bits left: the table never freezes.
    pub last: bool,
}

/// The regions of the host a step of [`Rows`] reads: the rows written of
/// each partition of the level, read at [`RowsEvent::Next`].
#[derive(Debug)]
pub struct RowsHost<'a> {
    pub written: &'a [u64],
}

const ROWS_TABLE: u32 = 1;
const ROWS_WRITING: u32 = 2;
const ROWS_READING: u32 = 3;
const ROWS_DONE: u32 = 4;

/// The machine of a table of a grouping's rows and of the level of rows
/// it writes once frozen.
///
/// It is plain data of a fixed layout, [`ROWS_WORDS`] words of 8 bytes.
/// A table's machine weighs the table after each batch
/// ([`RowsEvent::Grown`]): past `fill` eighths of the limit, with `least` groups
/// at least, and while bits are left, it freezes the table and plans the
/// level its rows go to ([`RowsAction::Freeze`]). Once the level's rows
/// are written ([`RowsEvent::Written`]), the host keeps the machine with
/// the level, and asks it for the partitions to read
/// ([`RowsEvent::Next`]): each with rows, in turn, and then the level is
/// done. The host reads the last level written first.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rows {
    params: RowsParams,
    phase: u32,
    /// The level a freeze writes, and its first hash bit; whether no
    /// level fits; the planned level's partitions, the first bit of a
    /// level below it and whether one fits; the next partition to read.
    level: u32,
    shift: u32,
    last: u32,
    partitions: u32,
    below: u32,
    deeper: u32,
    cursor: u32,
}

// Its size is part of the mirror a host keeps: words of 8 bytes.
const _: () = assert!(size_of::<Rows>() == 8 * ROWS_WORDS);

/// Words of 8 bytes a [`Rows`] takes.
pub const ROWS_WORDS: usize = 13;

impl Rows {
    /// The machine of the table of a grouping's input, whose rows go to
    /// the first level once frozen.
    ///
    /// A wrong call: a limit of 0 ([`Rule::JoinParams`]), and a shape no
    /// level can be planned by ([`plan::plan`]).
    pub fn new(params: &RowsParams) -> Result<Self> {
        ensure!(params.limit > 0, Error::wrong_call(Rule::JoinParams));
        plan::plan(
            &params.shape,
            &Level::first(usize::try_from(params.limit).unwrap_or(usize::MAX)),
        )?;
        Ok(Self {
            params: *params,
            phase: ROWS_TABLE,
            level: 0,
            shift: 0,
            last: 0,
            partitions: 0,
            below: 0,
            deeper: 0,
            cursor: 0,
        })
    }

    /// The machine of the table of a partition read back
    /// ([`RowsAction::Read`]).
    ///
    /// The wrong calls of [`Rows::new`].
    pub fn read(params: &RowsParams, table: &Table) -> Result<Self> {
        Ok(Self {
            level: table.level,
            shift: table.shift,
            last: u32::from(table.last),
            ..Self::new(params)?
        })
    }

    /// Take the host's event and give its next action.
    ///
    /// A wrong call: memory that is no machine ([`Rule::JoinState`]); an
    /// event its phase does not take ([`Rule::JoinEvent`]); counts of
    /// rows for another number of partitions ([`Rule::PartitionRows`]);
    /// and those of [`plan::plan`] for the level to freeze into. The
    /// machine is then as it was.
    pub fn step(&mut self, event: RowsEvent, host: &RowsHost<'_>) -> Result<RowsAction> {
        let mut next = *self;
        let action = next.take(event, host)?;
        *self = next;
        Ok(action)
    }

    fn take(&mut self, event: RowsEvent, host: &RowsHost<'_>) -> Result<RowsAction> {
        ensure!(
            (ROWS_TABLE..=ROWS_DONE).contains(&self.phase),
            Error::wrong_call(Rule::JoinState).with(self.phase)
        );
        match (self.phase, event) {
            (ROWS_TABLE, RowsEvent::Grown { memory, records }) => {
                if self.last != 0
                    || !full(self.params.limit, self.params.fill, memory)
                    || records < self.params.least
                {
                    return Ok(RowsAction::Done);
                }
                let plan = plan::plan(
                    &self.params.shape,
                    &Level {
                        shift: self.shift,
                        ..Level::first(usize::try_from(self.params.limit).unwrap_or(usize::MAX))
                    },
                )?;
                self.partitions = plan.partitions;
                self.below = plan.below;
                self.deeper = u32::from(plan.deeper);
                self.phase = ROWS_WRITING;
                Ok(RowsAction::Freeze {
                    plan,
                    level: self.level,
                })
            }
            (ROWS_WRITING, RowsEvent::Written) => {
                self.cursor = 0;
                self.phase = ROWS_READING;
                Ok(RowsAction::Done)
            }
            (ROWS_READING, RowsEvent::Next) => {
                ensure!(
                    host.written.len() == self.partitions as usize,
                    Error::wrong_call(Rule::PartitionRows)
                        .with(host.written.len())
                        .and(self.partitions)
                );
                while self.cursor < self.partitions {
                    let partition = self.cursor;
                    self.cursor += 1;
                    if host.written[partition as usize] > 0 {
                        let level = self
                            .level
                            .checked_add(1)
                            .ok_or(Error::wrong_call(Rule::JoinState).with(self.phase))?;
                        return Ok(RowsAction::Read(Table {
                            partition,
                            level,
                            shift: self.below,
                            last: self.deeper == 0,
                        }));
                    }
                }
                self.phase = ROWS_DONE;
                Ok(RowsAction::LevelDone)
            }
            (phase, event) => {
                Err(Error::wrong_call(Rule::JoinEvent)
                    .with(phase)
                    .and(match event {
                        RowsEvent::Grown { .. } => 1,
                        RowsEvent::Written => 2,
                        RowsEvent::Next => 3,
                    }))
            }
        }
    }
}

#[cfg(test)]
mod tests;
