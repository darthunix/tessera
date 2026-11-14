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
//!   Fits ──Inner──> Planned ──Made──> Building ──BuildEnd──> Probing ──OuterEnd──> Joining ──> Done
//!     │ (fits: Done)                   │    ^                    │                     │
//!     │                                │    │ Sent               │ Full (outer)        │ Next
//!     │                         Grown, │    │                    │                     │
//!     │                         Full   v    │                                          v
//!     │                               Sending                     a partition released, joined
//!                                                                 whole or in pieces, or split into a
//!                                                                 level below
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
//! - **Joining.** The host writes what each partition on disk still holds
//!   in memory and frees the partitions in memory, which the outer rows
//!   probed. Then it asks for the partitions on disk one at a time
//!   ([`Event::Next`]). One with no outer row, and no inner row the join
//!   gives out alone, is released ([`Action::Release`]). One too large
//!   for what the levels above leave of the limit, and much smaller than
//!   its level, so not one key, splits into a level below
//!   ([`Action::Split`]); the others are joined ([`Action::Open`]): whole,
//!   or, past two thirds of what the limit leaves, in pieces. When none
//!   is left, the level is done ([`Action::LevelDone`]).
//! - **Tables.** Before the table in memory goes, the level's table of
//!   the partitions in memory, a partition's or a piece's, the host
//!   reports that its outer rows are read ([`Event::TableEnd`]). Output
//!   it still holds that refers to the table goes out first
//!   ([`Action::Hold`]); then a join that gives out its inner rows
//!   without a pair gives out the table's ([`Action::Tail`]), once.
//! - **Pieces.** A partition joined in pieces is read a piece at a time
//!   ([`Event::Piece`]): the host reports the header of each block of its
//!   inner rows ([`Event::Block`]), and the machine says whether the
//!   block joins the piece or starts the next ([`Action::Carry`]). A
//!   piece ends past its room, where a group of blocks ends: values
//!   followed by the records that refer to them, so that no record is
//!   read apart from its values. After each piece the host reads the
//!   partition's outer rows ([`Event::PassEnd`]), and marks those with a
//!   pair; a join that gives out its outer rows without a pair reads them
//!   once more, without a table ([`Action::FinalPass`]).
//!
//! An event that the machine's phase does not take is a wrong call
//! ([`Rule::MachineEvent`]), and the machine stays as it was. The machine
//! decides by the numbers the host gives: its memory, its bytes and its
//! rows. It does nothing for each row: the host puts the rows into their
//! partitions, and where a decision is made for each row, the machine
//! makes it once for each partition, as the routes of the outer rows.
//!
//! A level below, which splits a partition of a level too large, is a
//! machine of its own ([`Join::below`]), which starts planned: the host
//! makes it, splits the partition's inner rows into it, probes it with
//! the partition's outer rows, and joins its partitions; then it asks
//! the level above for its next partition.
//!
//! The machine holds no memory: it is plain data, which the host keeps
//! where it likes, and every region it reads comes in a [`Host`] at each
//! step. It allocates nothing.

use core::cell::Cell;

use crate::columns;
use crate::error::ensure;
use crate::plan::{self, Level, Plan, Shape};
use crate::shared_spill::{LocalSpill, Memory, Partition, Records, SplitWeights, Weights, splits};
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
    /// 1 when the join gives out the inner rows without a pair, as a RIGHT
    /// or a FULL join does, 0 otherwise: a partition with inner rows and
    /// no outer row is joined then, not released.
    pub preserve_inner: u32,
    /// 1 when the join gives out the outer rows without a pair, as a LEFT,
    /// a FULL or an ANTI join does, 0 otherwise: a partition read in
    /// pieces ends with a pass of its outer rows without a table, which
    /// gives out those no piece paired.
    pub unmatched_outer: u32,
    /// Zero.
    pub spare: u32,
    /// The bytes of the buffer a file of outer rows keeps.
    pub page: u64,
    /// The weights of the rule that sends partitions to disk, as
    /// [`Weights`] names them: `start`, `target`, `spilled` and
    /// `resident`;
    pub start: f64,
    /// see `start`;
    pub target: f64,
    /// see `start`;
    pub spilled: f64,
    /// see `start`;
    pub resident: f64,
    /// and the share of the outer tail that each partition on disk keeps
    /// in reserve: its chunk of outer rows, its chunk of values when they
    /// keep values, and the page of its file.
    pub tail: f64,
    /// The weights of the rule that splits a partition on disk into a
    /// level below, as [`SplitWeights`] names them: `room`;
    pub split_room: f64,
    /// and `key`.
    pub split_key: f64,
}

/// A step of the host: what it saw.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Event {
    /// A batch of inner rows is about to go into the table in memory,
    /// which takes `held` bytes in `rows` rows of the `estimate` the host
    /// expects; with the batch, its index and the chunks it may add, it
    /// would take `needed`.
    Inner {
        /// The bytes the table takes.
        held: u64,
        /// Its rows.
        rows: u64,
        /// The rows the host expects.
        estimate: f64,
        /// The bytes it would take with the batch.
        needed: u64,
    },
    /// The level of the last [`Action::Start`] is made, with its spill
    /// words.
    Made,
    /// A partition of one side has no room for its next row, or its
    /// values passed a chunk; `tail` when it holds a chunk of rows in
    /// memory.
    Full {
        /// The side of the partition.
        side: Side,
        /// The partition.
        partition: u32,
        /// Whether it holds a chunk of rows in memory.
        tail: bool,
    },
    /// Rows went into the partitions: the host's memory now.
    Grown {
        /// The bytes the host holds.
        memory: u64,
    },
    /// The partition of the last [`Action::Demote`] or [`Action::Evict`]
    /// went to disk: whether that freed memory, and the memory now.
    Sent {
        /// Whether its bytes in memory went down.
        freed: bool,
        /// The bytes the host holds.
        memory: u64,
    },
    /// The inner rows are read.
    BuildEnd,
    /// The outer rows are read.
    OuterEnd,
    /// The host asks for the next partition on disk: the one it took is
    /// done, or none was taken yet. `memory` is what its spill holds, the
    /// levels above included, besides a partition it would join.
    Next {
        /// The bytes the spill holds, as said above.
        memory: u64,
    },
    /// A participant of a shared build took `partition` to join alone:
    /// whether it splits, with `memory` as for [`Event::Next`].
    Take {
        /// The partition taken.
        partition: u32,
        /// The bytes the spill holds.
        memory: u64,
    },
    /// The host starts the next piece of a partition read in pieces:
    /// `memory` as for [`Event::Next`], the last piece dropped.
    Piece {
        /// The bytes the spill holds.
        memory: u64,
    },
    /// The header of the partition's next block of inner rows, a block of
    /// values or of records, of `len` bytes: whether it joins the piece.
    Block {
        /// Whether it is a block of values.
        values: bool,
        /// Its bytes.
        len: u64,
    },
    /// The partition's inner rows on disk are all read.
    FileEnd,
    /// A pass over the partition's outer rows ended.
    PassEnd,
    /// The outer rows of the table in memory are read: the level's table
    /// of the partitions in memory, a partition's or a piece's. `held`
    /// when the host still holds output of that table that has not gone
    /// out, such as pairs it copied, which refer to it.
    TableEnd {
        /// Whether output of the table is still held.
        held: bool,
    },
    /// The tail of the last [`Action::Tail`] went out.
    TailEnd,
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
    /// This partition has nothing to join: free it and its files, then ask
    /// for the next.
    Release(u32),
    /// Join this partition, its inner rows read back as a table and its
    /// outer rows probing it: whole, then ask for the next; or, with
    /// `pieces`, a piece at a time ([`Event::Piece`]), its outer rows read
    /// once a piece.
    Open {
        /// The partition to join.
        partition: u32,
        /// Whether it is joined a piece at a time.
        pieces: bool,
    },
    /// Read the block into the piece, and its header's next.
    Load,
    /// The piece ends before this block, which the next piece starts
    /// with: probe the piece with the partition's outer rows, then report
    /// [`Event::PassEnd`].
    Carry,
    /// Drop the piece and start the next ([`Event::Piece`]).
    NextPiece,
    /// Drop the piece and read the partition's outer rows once more,
    /// without a table: those no piece paired go out. Then report
    /// [`Event::PassEnd`].
    FinalPass,
    /// The table may not go yet: give out the output held, then report
    /// the end of the table again.
    Hold,
    /// Give out the table's inner rows without a pair, its tail, then
    /// report [`Event::TailEnd`] and the end of the table again.
    Tail,
    /// Split a partition into a level below: make it, its machine by
    /// [`Join::below`].
    Split(Below),
    /// Every partition of the level is done.
    LevelDone,
}

/// A level below to make, which splits a partition of the level above.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Below {
    /// The partition of the level above that it splits.
    pub partition: u32,
    /// Its level, one past the level above's.
    pub level: u32,
    /// The bytes it expects: the partition's on disk, where all of it is
    /// once its level joins.
    pub expected: u64,
    /// The memory it may take: what the levels above leave of the limit,
    /// a quarter of it at least.
    pub limit: u64,
    /// The first hash bit it takes, past those of the levels above.
    pub shift: u32,
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
    /// The inner rows of each partition.
    pub rows: &'a [u64],
    /// The inner rows of the level.
    pub total: u64,
    /// The outer rows written of each partition, read at
    /// [`Event::Next`].
    pub outer: &'a [u64],
    /// The bytes the blocks of inner rows on disk of each partition take
    /// read back, unpacked, which a table of them holds, not those they
    /// take on disk; read at [`Event::Next`].
    pub disk: &'a [u64],
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
const DONE: u32 = 8;
const SHARED: u32 = 9;
const OPENED: u32 = 10;
const LOADING: u32 = 11;
const PASS: u32 = 12;
const TAIL: u32 = 13;

/// The flags of a partition read in pieces: its file is read to the end;
/// its last pass, without a table, runs; the piece's last block is one of
/// records; the piece has a block of values. And those of the table in
/// memory: there is one, whose outer rows the host reads; its tail went
/// out.
const FILE_DONE: u32 = 1;
const FINAL: u32 = 2;
const RECORDS: u32 = 4;
const VALUES: u32 = 8;
const TABLE: u32 = 16;
const TAIL_GIVEN: u32 = 32;

/// The machine of one level of a join's spill.
///
/// It is plain data of a fixed layout, [`JOIN_WORDS`] words of 8 bytes,
/// so that a host in another language keeps it in memory of its own; a
/// step checks what it reads, and memory that is no machine is a wrong
/// call ([`Rule::MachineState`]).
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
    /// The next partition to join.
    cursor: u32,
    /// A partition read in pieces: the bytes a piece may take, and those
    /// it took; the partition, the phase its level goes back to after it,
    /// the flags, and the phase a tail goes back to.
    room: u64,
    loaded: u64,
    current: u32,
    home: u32,
    flags: u32,
    back: u32,
}

// Its size is part of the mirror a host keeps: words of 8 bytes.
const _: () = assert!(size_of::<Join>() == 8 * JOIN_WORDS);

/// Words of 8 bytes a [`Join`] takes.
pub const JOIN_WORDS: usize = 31;

impl Join {
    /// The machine of a join whose table fits so far.
    ///
    /// A wrong call: a limit of 0 or weights below zero or not finite
    /// ([`Rule::MachineParams`]); outer rows of more words than a chunk of
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
            cursor: 0,
            room: 0,
            loaded: 0,
            current: 0,
            home: 0,
            flags: 0,
            back: 0,
        })
    }

    /// The machine of a level below that [`Action::Split`] asked for, and
    /// the level to make; it waits for [`Event::Made`].
    ///
    /// The wrong calls of [`Join::new`], a level 0, and those of
    /// [`plan::plan`].
    pub fn below(params: &Params, below: &Below) -> Result<(Self, Start)> {
        ensure!(
            below.level > 0,
            Error::wrong_call(Rule::LevelBelow).with(below.level)
        );
        let mut join = Self::new(params)?;
        let start =
            join.plan_level(below.level, below.expected as f64, below.limit, below.shift)?;
        Ok((join, start))
    }

    /// The first level of a build that several processes share, whose
    /// `partitions` one of them planned: the level each of the others
    /// makes, with its chunks of outer rows, and its machine. The shared
    /// build decides the level, but for whether a partition a participant
    /// takes alone splits ([`Event::Take`]).
    ///
    /// The wrong calls of [`Join::new`] and those of [`plan::plan`] for
    /// given partitions.
    pub fn given(params: &Params, partitions: u32) -> Result<(Self, Start)> {
        let mut join = Self::new(params)?;
        let start = join.plan_given(0, 0.0, params.limit, 0, partitions)?;
        join.phase = SHARED;
        Ok((join, start))
    }

    /// The level: 0 for the first.
    pub fn level(&self) -> u32 {
        self.level
    }

    /// Take the host's event and give its next action.
    ///
    /// A wrong call: memory that is no machine ([`Rule::MachineState`]); an
    /// event its phase does not take ([`Rule::MachineEvent`]); a partition
    /// past the level's ([`Rule::Partition`]); spill words that are not
    /// the level's ([`Rule::SpillWords`]); counts of rows or routes for
    /// another number of partitions ([`Rule::RecordCounts`],
    /// [`Rule::JoinRoutes`]); and those of [`plan::plan`] for the level
    /// to start. The machine is then as it was, though a refused
    /// [`Event::Made`] may have cleared the words.
    pub fn step(&mut self, event: Event, host: &mut Host<'_>) -> Result<Action> {
        let phase = self.phase()?;
        // No table goes, nor does another come, before its end is reported.
        let ended = self.flags & TABLE == 0;
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
                // The end weighs no memory, but its shares still go from
                // start to target: a start of 0 sends a partition whatever
                // the memory, and a target of more stops.
                self.send(host, None, self.evicted + 1)
            }
            (PROBING, Event::OuterEnd) if ended => {
                self.phase = JOINING;
                self.cursor = 0;
                Ok(Action::Done)
            }
            (JOINING, Event::Next { memory }) if ended => self.next(host, memory),
            (SHARED, Event::Take { partition, memory }) if ended => {
                self.counts(host)?;
                ensure!(
                    partition < self.partitions,
                    Error::wrong_call(Rule::Partition)
                        .with(partition)
                        .and(self.partitions)
                );
                self.take(host, partition, memory)
            }
            (OPENED, Event::Piece { memory }) => {
                self.room = self.room_of(memory);
                self.loaded = 0;
                self.flags &= !(RECORDS | VALUES);
                self.phase = LOADING;
                Ok(Action::Done)
            }
            (LOADING, Event::Block { values, len }) => {
                // A group of blocks, values and the records that refer to
                // them, ends where values follow records: a piece ends
                // there, or at any block of records while it has no values.
                if self.flags & RECORDS != 0
                    && self.loaded >= self.room
                    && (values || self.flags & VALUES == 0)
                {
                    self.phase = PASS;
                    self.flags |= TABLE;
                    return Ok(Action::Carry);
                }
                self.loaded = self.loaded.saturating_add(len);
                self.flags = if values {
                    (self.flags & !RECORDS) | VALUES
                } else {
                    self.flags | RECORDS
                };
                Ok(Action::Load)
            }
            (LOADING, Event::FileEnd) => {
                self.flags |= FILE_DONE | TABLE;
                self.phase = PASS;
                Ok(Action::Done)
            }
            (PROBING | JOINING | SHARED | PASS | DONE, Event::TableEnd { held }) => {
                if held {
                    return Ok(Action::Hold);
                }
                // A join that gives out its inner rows without a pair gives
                // out those of each table before the table goes.
                if self.flags & (TABLE | TAIL_GIVEN) == TABLE && self.params.preserve_inner != 0 {
                    self.back = phase;
                    self.phase = TAIL;
                    return Ok(Action::Tail);
                }
                self.flags &= !(TABLE | TAIL_GIVEN);
                Ok(Action::Done)
            }
            (TAIL, Event::TailEnd) => {
                self.flags |= TAIL_GIVEN;
                self.phase = self.back;
                Ok(Action::Done)
            }
            (PASS, Event::PassEnd) if ended => {
                if self.flags & FILE_DONE == 0 {
                    self.phase = OPENED;
                    return Ok(Action::NextPiece);
                }
                if self.params.unmatched_outer != 0 && self.flags & FINAL == 0 {
                    self.flags |= FINAL;
                    return Ok(Action::FinalPass);
                }
                self.phase = self.home;
                Ok(Action::Done)
            }
            _ => Err(Error::wrong_call(Rule::MachineEvent)
                .with(phase)
                .and(code(&event))),
        }
    }

    /// The phase, checked.
    fn phase(&self) -> Result<u32> {
        ensure!(
            (FITS..=TAIL).contains(&self.phase),
            Error::wrong_call(Rule::MachineState).with(self.phase)
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
        let mut tail = (outer_len as u64).saturating_add(self.params.page);
        if self.params.outer_values != 0 {
            tail = tail.saturating_add(outer_len as u64);
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

    /// The next partition on disk from the cursor: released, split or
    /// joined whole; or the end of the level. `memory` is what the spill
    /// holds besides a partition to join.
    fn next(&mut self, host: &Host<'_>, memory: u64) -> Result<Action> {
        let spill = self.spill(host)?;
        self.counts(host)?;
        let mut cursor = self.cursor;
        while cursor < self.partitions {
            let partition = cursor;
            let index = partition as usize;
            cursor += 1;
            // The partitions in memory went as the level started joining.
            if !spill.on_disk(partition)? {
                continue;
            }
            let action = if host.outer[index] == 0
                && (self.params.preserve_inner == 0 || host.rows[index] == 0)
            {
                Action::Release(partition)
            } else {
                self.take(host, partition, memory)?
            };
            self.cursor = cursor;
            return Ok(action);
        }
        self.cursor = cursor;
        self.phase = DONE;
        Ok(Action::LevelDone)
    }

    /// Refuse counts of rows, outer rows or bytes on disk for another
    /// number of partitions than the level's.
    fn counts(&self, host: &Host<'_>) -> Result<()> {
        let partitions = self.partitions as usize;
        for (what, len) in [
            (Rule::RecordCounts, host.rows.len()),
            (Rule::JoinOuter, host.outer.len()),
            (Rule::DiskCounts, host.disk.len()),
        ] {
            ensure!(
                len == partitions,
                Error::wrong_call(what).with(len).and(partitions)
            );
        }
        Ok(())
    }

    /// A partition on disk to join: split into a level below when one of
    /// the level's shape fits in the bits left, it is larger than `room`
    /// of what the limit leaves besides `memory`, a chunk of outer rows
    /// and its values, and it holds fewer than `key` of its level's rows,
    /// so not one key; whole or in pieces otherwise.
    fn take(&mut self, host: &Host<'_>, partition: u32, memory: u64) -> Result<Action> {
        let index = partition as usize;
        let used = memory.saturating_add(self.outer_len.saturating_mul(2));
        let split = self.deeper != 0
            && splits(
                &SplitWeights {
                    room: self.params.split_room,
                    key: self.params.split_key,
                },
                &Partition {
                    size: host.disk[index],
                    used,
                    limit: self.params.limit,
                    rows: host.rows[index],
                    level_rows: host.total,
                    bits: self.below,
                },
            )?;
        if !split {
            // A file larger than the room of a piece is read in pieces.
            let pieces = host.disk[index] > self.room_of(memory);
            if pieces {
                self.home = self.phase;
                self.phase = OPENED;
                self.current = partition;
                self.flags = 0;
            } else {
                self.flags = TABLE;
            }
            return Ok(Action::Open { partition, pieces });
        }
        Ok(Action::Split(Below {
            partition,
            level: self.level + 1,
            expected: host.disk[index],
            limit: plan::below_limit(self.params.limit, memory),
            shift: self.below,
        }))
    }

    /// The bytes a piece of a partition may take: two thirds of what the
    /// limit leaves besides `memory` and a chunk of outer rows and its
    /// values, the rest for the piece's index.
    fn room_of(&self, memory: u64) -> u64 {
        let used = memory.saturating_add(self.outer_len.saturating_mul(2));
        self.params.limit.saturating_sub(used) / 3 * 2
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
        // The partitions in memory make the table the outer rows probe.
        self.flags = TABLE;
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
        params.split_room,
        params.split_key,
    ];
    ensure!(
        params.limit > 0
            && params.preserve_inner <= 1
            && params.unmatched_outer <= 1
            && params.spare == 0
            && weights
                .iter()
                .all(|weight| weight.is_finite() && *weight >= 0.0),
        Error::wrong_call(Rule::MachineParams)
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

/// The number of an event in a [`Rule::MachineEvent`].
fn code(event: &Event) -> u32 {
    match event {
        Event::Inner { .. } => 1,
        Event::Made => 2,
        Event::Full { .. } => 3,
        Event::Grown { .. } => 4,
        Event::Sent { .. } => 5,
        Event::BuildEnd => 6,
        Event::OuterEnd => 7,
        Event::Next { .. } => 8,
        Event::Take { .. } => 9,
        Event::Piece { .. } => 10,
        Event::Block { .. } => 11,
        Event::FileEnd => 12,
        Event::PassEnd => 13,
        Event::TableEnd { .. } => 14,
        Event::TailEnd => 15,
    }
}

#[cfg(test)]
mod model;
#[cfg(test)]
mod tests;
