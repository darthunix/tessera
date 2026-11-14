//! The machine of a sort's runs.
//!
//! A sort keeps its rows in memory while they fit its memory limit.
//! Past it, it sorts them and writes them to disk as a run, a sorted part
//! of its input, and starts again. Once the input ends, the runs merge,
//! many at once, pass after pass, until one merge reads them all as the
//! sort's output. The machine is the plan of that spill: when a run goes
//! to disk, where it is written, which runs each pass merges, and how
//! many runs the last merge reads. The host, the sort, holds it and steps
//! it with what it saw ([`Event`]), and does what the machine answers
//! ([`Action`]). The merge of the rows itself is the host's.
//!
//! ```text
//!   Rows ──InputEnd──> Ending ──> Planning ──> Reading ──Rescan──> Reading
//!                                  │    ^
//!                             Next v    │ PassEnd
//!                                 Merging
//! ```
//!
//! - **Rows.** After each batch the host reports the bytes its rows take,
//!   how many there are, and the bytes it may take now ([`Event::Grown`]).
//!   With what sorting them takes and a chunk more, which the next batch
//!   may need, past that limit, the rows go to a run ([`Action::Run`]).
//! - **Ending.** At the end of the input ([`Event::InputEnd`]) the rows
//!   left go to a last run. A sort that wrote no run sorts in memory and
//!   is done.
//! - **Planning.** While there are more runs than one merge reads, a pass
//!   merges them ([`Action::Pass`]); a sort read backward merges them
//!   into one, which it reads by blocks.
//! - **Merging.** The pass merges its runs in turn, as many at once as a
//!   merge reads, each group into a new run ([`Action::Merge`]); a run
//!   left alone goes on as it is ([`Action::Keep`]). Then the pass ends
//!   ([`Action::PassEnd`]) and the new runs are the runs.
//! - **Reading.** The last merge reads the runs left
//!   ([`Action::Read`]). A rescan reads them again ([`Event::Rescan`]).
//!
//! The runs go into sets: a set is one file of the host's, which holds
//! up to `set_runs` runs, each a partition of it. The runs of the input go
//! into one set until it is full, those of each pass into others, so a
//! set never holds runs of two passes and can go once its runs are read.
//! The machine gives each run its set and partition ([`Place`]).
//!
//! An event that the machine's phase does not take is a wrong call
//! ([`Rule::JoinEvent`]), and the machine stays as it was. The machine
//! does nothing for each row, holds no memory and allocates nothing: it
//! is plain data the host keeps.

use crate::error::ensure;
use crate::{Error, Result, Rule};

/// The fewest runs a merge reads at once, however small the limit:
/// fewer would take a pass for every few runs, each reading and writing
/// every row again, and a block of a run is small then anyway.
pub const MIN_MERGE: u32 = 6;

/// What a sort's runs share, from the start of its input to its end.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Params {
    /// The bytes the sort may take when it starts, by which the blocks
    /// of its runs and the runs a merge reads are planned; a batch brings
    /// the limit its rows weigh by ([`Event::Grown`]).
    pub limit: u64,
    /// The bytes of a row in a run's block.
    pub row: u64,
    /// The bytes sorting a row in memory takes besides the row: its item
    /// and its reference.
    pub item: u64,
    /// The bytes of a chunk of rows in memory, which the next batch may
    /// need.
    pub chunk: u64,
    /// The runs a set holds.
    pub set_runs: u32,
    /// The most runs a merge reads at once, at least 2.
    pub max_merge: u32,
    /// The rows a block holds at least: a batch of the host's.
    pub batch_rows: u32,
    /// 1 when the output is read backward too: the runs merge into one,
    /// read by blocks; else 0.
    pub backward: u32,
}

/// The blocks of a run and the runs a merge reads, by [`Params`].
///
/// A merge holds a block of each run it reads: a block of rows, and one
/// of the by-reference values they point to. A block takes a 128th of
/// the limit, up to 256 KiB, and at least a batch of rows and 4 KiB of
/// values; a merge reads what the limit holds of two blocks each, at
/// least [`MIN_MERGE`] runs and at most `max_merge`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Blocks {
    /// The rows a block of a run holds.
    pub rows: u32,
    /// The bytes of values a block of a run holds.
    pub values: u64,
    /// The runs a merge reads at once.
    pub fan_in: u32,
}

/// Where a run is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Place {
    /// A new set holds it: the set being written, if any, ends first.
    pub create: bool,
    /// Its partition of the set.
    pub partition: u32,
}

/// What a host of a sort's runs reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// A batch went in: its rows in memory take `memory` bytes, there are
    /// `rows` of them, and the sort may take `limit` bytes now, which the
    /// host may change from one batch to the next.
    Grown {
        /// The bytes the rows in memory take.
        memory: u64,
        /// The rows in memory.
        rows: u64,
        /// The bytes the sort may take now.
        limit: u64,
    },
    /// The input ended, `rows` rows left in memory.
    InputEnd {
        /// The rows left in memory.
        rows: u64,
    },
    /// The host asks for the next step of the merge.
    Next,
    /// The output is read again from its start.
    Rescan,
}

/// What the host of a sort's runs does next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Nothing more: the step's event is done.
    Done,
    /// Sort the rows in memory and write them as a run at this place,
    /// after the runs written before it; memory is free again.
    Run(Place),
    /// A pass starts: the set being written ends, and the runs, numbered
    /// from 0 in the order they were written, are the pass's to read.
    Pass,
    /// Merge `count` runs of the pass from `first` into a new run at
    /// `place`, after the pass's runs written before it, and free them.
    Merge {
        /// The first run merged.
        first: u32,
        /// The runs merged.
        count: u32,
        /// Where the new run goes.
        place: Place,
    },
    /// Run `run` of the pass goes on as it is, after the pass's runs
    /// written before it.
    Keep {
        /// The run kept.
        run: u32,
    },
    /// The pass ends: the runs it wrote and kept are the runs.
    PassEnd,
    /// The set being written ends, and the last merge reads the `runs`
    /// runs from their start; one run is read by blocks.
    Read {
        /// The runs the last merge reads.
        runs: u32,
    },
}

const ROWS: u32 = 1;
const ENDING: u32 = 2;
const PLANNING: u32 = 3;
const MERGING: u32 = 4;
const READING: u32 = 5;
const DONE: u32 = 6;

/// The machine of a sort's runs.
///
/// One machine serves one reading of the sort's input; a sort that reads
/// its input anew, as a rescan with other parameters does, starts with a
/// new one ([`Runs::new`]). It is plain data of a fixed layout,
/// [`RUNS_WORDS`] words of 8 bytes.
/// Any bytes may be given to [`Runs::step`]: those that are no machine
/// are a wrong call ([`Rule::JoinState`]).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Runs {
    params: Params,
    phase: u32,
    /// The runs a merge reads at once.
    fan_in: u32,
    /// The runs: written, or of the pass being written.
    runs: u32,
    /// Whether a set is being written, and its runs.
    open: u32,
    used: u32,
    /// The pass's runs, and the first not yet merged or kept.
    pass_runs: u32,
    done: u32,
    spare: u32,
}

// Its size is part of the mirror a host keeps: words of 8 bytes.
const _: () = assert!(size_of::<Runs>() == 8 * RUNS_WORDS);

/// Words of 8 bytes a [`Runs`] takes.
pub const RUNS_WORDS: usize = 10;

impl Runs {
    /// The machine of a sort's runs, before its input, and the blocks of
    /// its runs.
    ///
    /// A wrong call ([`Rule::JoinParams`]): a limit, a row's bytes, a
    /// set's runs or a batch of 0; a merge of fewer than 2 runs; or a
    /// `backward` neither 0 nor 1.
    pub fn new(params: &Params) -> Result<(Self, Blocks)> {
        ensure!(counted(params), Error::wrong_call(Rule::JoinParams));
        let blocks = blocks(params);
        Ok((
            Self {
                params: *params,
                phase: ROWS,
                fan_in: blocks.fan_in,
                runs: 0,
                open: 0,
                used: 0,
                pass_runs: 0,
                done: 0,
                spare: 0,
            },
            blocks,
        ))
    }

    /// Take the host's event and give its next action.
    ///
    /// A wrong call: memory that is no machine ([`Rule::JoinState`]); an
    /// event its phase does not take ([`Rule::JoinEvent`]); and more runs
    /// than a `u32` counts ([`Rule::JoinState`]). The machine is then as
    /// it was.
    pub fn step(&mut self, event: Event) -> Result<Action> {
        self.check()?;
        let mut next = *self;
        let action = next.take(event)?;
        *self = next;
        Ok(action)
    }

    fn take(&mut self, event: Event) -> Result<Action> {
        match (self.phase, event) {
            (
                ROWS,
                Event::Grown {
                    memory,
                    rows,
                    limit,
                },
            ) => {
                let sorting = rows.saturating_mul(self.params.item);
                let bytes = memory
                    .saturating_add(sorting)
                    .saturating_add(self.params.chunk);
                if rows == 0 || bytes <= limit {
                    return Ok(Action::Done);
                }
                self.run()
            }
            (ROWS, Event::InputEnd { rows }) => {
                if self.runs == 0 {
                    self.phase = DONE;
                    return Ok(Action::Done);
                }
                self.phase = ENDING;
                if rows > 0 {
                    return self.run();
                }
                self.plan()
            }
            (ENDING | PLANNING, Event::Next) => self.plan(),
            (MERGING, Event::Next) => {
                if self.done == self.pass_runs {
                    self.phase = PLANNING;
                    return Ok(Action::PassEnd);
                }
                let first = self.done;
                let count = self.fan_in.min(self.pass_runs - first);
                self.done += count;
                if count == 1 {
                    self.runs = self
                        .runs
                        .checked_add(1)
                        .ok_or(Error::wrong_call(Rule::JoinState).with(self.phase))?;
                    return Ok(Action::Keep { run: first });
                }
                let place = self.place()?;
                Ok(Action::Merge {
                    first,
                    count,
                    place,
                })
            }
            (READING, Event::Rescan) => Ok(Action::Read { runs: self.runs }),
            (phase, event) => {
                Err(Error::wrong_call(Rule::JoinEvent)
                    .with(phase)
                    .and(match event {
                        Event::Grown { .. } => 1,
                        Event::InputEnd { .. } => 2,
                        Event::Next => 3,
                        Event::Rescan => 4,
                    }))
            }
        }
    }

    /// The machine's words, checked: a phase it has, a merge of 2 runs at
    /// least, and counts within what they count.
    fn check(&self) -> Result<()> {
        let params = &self.params;
        ensure!(
            counted(params)
                && (ROWS..=DONE).contains(&self.phase)
                && (2..=params.max_merge).contains(&self.fan_in)
                && self.open <= 1
                && self.used <= params.set_runs
                && self.done <= self.pass_runs,
            Error::wrong_call(Rule::JoinState).with(self.phase)
        );
        Ok(())
    }

    /// A new run.
    fn run(&mut self) -> Result<Action> {
        Ok(Action::Run(self.place()?))
    }

    /// A new run, placed after the others: in the set being written while
    /// it has room, else in a new one.
    fn place(&mut self) -> Result<Place> {
        self.runs = self
            .runs
            .checked_add(1)
            .ok_or(Error::wrong_call(Rule::JoinState).with(self.phase))?;
        let create = self.open == 0 || self.used == self.params.set_runs;
        if create {
            self.open = 1;
            self.used = 0;
        }
        let partition = self.used;
        self.used += 1;
        Ok(Place { create, partition })
    }

    /// After the input or a pass: another pass while one merge cannot
    /// read every run, else the last merge.
    fn plan(&mut self) -> Result<Action> {
        // The runs written so far are read now: the set writes no more.
        self.open = 0;
        let most = if self.params.backward != 0 {
            1
        } else {
            self.fan_in
        };
        if self.runs <= most {
            self.phase = READING;
            return Ok(Action::Read { runs: self.runs });
        }
        self.pass_runs = self.runs;
        self.done = 0;
        self.runs = 0;
        self.phase = MERGING;
        Ok(Action::Pass)
    }
}

/// Whether `params` are those of a sort: no limit, row, set or batch of
/// 0, a merge of 2 runs at least, and a `backward` of 0 or 1.
fn counted(params: &Params) -> bool {
    params.limit > 0
        && params.row > 0
        && params.set_runs > 0
        && params.batch_rows > 0
        && params.max_merge >= 2
        && params.backward <= 1
}

/// The blocks of a run and the runs a merge reads, by `params`.
fn blocks(params: &Params) -> Blocks {
    let block = (params.limit / 128).min(256 * 1024);
    let rows = (block / params.row).max(u64::from(params.batch_rows));
    let values = block.max(4096);
    let block = block.max(rows.saturating_mul(params.row));
    let fan_in = (params.limit / block.saturating_mul(2))
        .max(u64::from(MIN_MERGE))
        .min(u64::from(params.max_merge));
    Blocks {
        rows: u32::try_from(rows).unwrap_or(u32::MAX),
        values,
        fan_in: u32::try_from(fan_in).unwrap_or(params.max_merge),
    }
}

#[cfg(test)]
mod tests;
