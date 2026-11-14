//! The machine of a join's spill ([`relspill::join`]), declared in
//! `include/tessera/spill.h`. The join keeps a machine for each level of
//! its spill, and the regions a step reads, and steps it with the events
//! of its build.

use std::cell::Cell;
use std::ffi::c_int;
use std::mem::{offset_of, size_of};

use anyhow::{Context, Result, bail, ensure};
use relspill::join::{Action, Below, Event, Host, Join, Params, Side, Start};
use relspill::plan::Shape;

use super::shared_spill::check_spill_words;
use super::spill::{SpillPlan, SpillShape};
use super::status::{Code, Status, guard};
use super::table::{slots, values};

/// `TessJoinParams`: what every level of a join's spill shares; see
/// [`Params`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct JoinParams {
    pub limit: u64,
    pub shape: SpillShape,
    pub outer_words: u64,
    pub outer_values: u32,
    pub preserve_inner: u32,
    pub unmatched_outer: u32,
    pub spare: u32,
    pub page: u64,
    pub start: f64,
    pub target: f64,
    pub spilled: f64,
    pub resident: f64,
    pub tail: f64,
    pub split_room: f64,
    pub split_key: f64,
}

impl JoinParams {
    fn params(&self) -> Params {
        let shape = &self.shape;
        Params {
            limit: self.limit,
            shape: Shape {
                reserve: shape.reserve,
                min_partitions: shape.min_partitions,
                max_partitions: shape.max_partitions,
                share: shape.share,
                min_chunk: shape.min_chunk,
                max_chunk: shape.max_chunk,
                record: shape.record,
            },
            outer_words: self.outer_words,
            outer_values: self.outer_values,
            preserve_inner: self.preserve_inner,
            unmatched_outer: self.unmatched_outer,
            spare: self.spare,
            page: self.page,
            start: self.start,
            target: self.target,
            spilled: self.spilled,
            resident: self.resident,
            tail: self.tail,
            split_room: self.split_room,
            split_key: self.split_key,
        }
    }
}

/// `TessJoinHost`: the regions of a level a step reads; see [`Host`].
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct JoinHost {
    pub words: *mut u64,
    pub nwords: usize,
    pub rows: *const u64,
    pub nrows: usize,
    pub total: u64,
    pub outer: *const u64,
    pub nouter: usize,
    pub disk: *const u64,
    pub ndisk: usize,
    pub routes: *mut u8,
    pub nroutes: usize,
}

/// `TessJoinEvent`: a step of the join; see [`Event`]. `partition` is
/// that of [`JOIN_FULL`] and [`JOIN_TAKE`]; `flag` the tail of
/// [`JOIN_FULL`], the bytes freed of [`JOIN_SENT`], the values of
/// [`JOIN_BLOCK`] and the output held of [`JOIN_TABLE_END`]; `memory` the bytes needed of [`JOIN_INNER`] and the
/// memory of the others; `len` the bytes of [`JOIN_BLOCK`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct JoinEvent {
    pub kind: u32,
    pub side: u32,
    pub partition: u32,
    pub flag: u32,
    pub held: u64,
    pub rows: u64,
    pub estimate: f64,
    pub memory: u64,
    pub len: u64,
}

/// `TessJoinEventKind`, of a [`JoinEvent`].
pub const JOIN_INNER: u32 = 1;
pub const JOIN_MADE: u32 = 2;
pub const JOIN_FULL: u32 = 3;
pub const JOIN_GROWN: u32 = 4;
pub const JOIN_SENT: u32 = 5;
pub const JOIN_BUILD_END: u32 = 6;
pub const JOIN_OUTER_END: u32 = 7;
pub const JOIN_NEXT: u32 = 8;
pub const JOIN_TAKE: u32 = 9;
pub const JOIN_PIECE: u32 = 10;
pub const JOIN_BLOCK: u32 = 11;
pub const JOIN_FILE_END: u32 = 12;
pub const JOIN_PASS_END: u32 = 13;
pub const JOIN_TABLE_END: u32 = 14;
pub const JOIN_TAIL_END: u32 = 15;

/// `TessJoinBelow`: a level below to make; see [`Below`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JoinBelow {
    pub partition: u32,
    pub level: u32,
    pub expected: u64,
    pub limit: u64,
    pub shift: u32,
    pub spare: u32,
}

impl JoinBelow {
    fn below(&self) -> Result<Below> {
        ensure!(
            self.spare == 0,
            "a level below with a spare word of {}",
            self.spare
        );
        Ok(Below {
            partition: self.partition,
            level: self.level,
            expected: self.expected,
            limit: self.limit,
            shift: self.shift,
        })
    }
}

/// `TessJoinAction`: what the join does next; see [`Action`]. The plan
/// and `outer_len` are those of [`JOIN_START`], `partition` that of the
/// actions on a partition, `pieces` that of [`JOIN_OPEN`], and `below`
/// that of [`JOIN_SPLIT`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JoinAction {
    pub kind: u32,
    pub partition: u32,
    pub pieces: u32,
    pub spare: u32,
    pub outer_len: usize,
    pub plan: SpillPlan,
    pub below: JoinBelow,
}

/// `TessJoinActionKind`, of a [`JoinAction`].
pub const JOIN_DONE: u32 = 0;
pub const JOIN_START: u32 = 1;
pub const JOIN_ADD_CHUNK: u32 = 2;
pub const JOIN_FLUSH: u32 = 3;
pub const JOIN_DEMOTE: u32 = 4;
pub const JOIN_EVICT: u32 = 5;
pub const JOIN_RELEASE: u32 = 6;
pub const JOIN_OPEN: u32 = 7;
pub const JOIN_SPLIT: u32 = 8;
pub const JOIN_LEVEL_DONE: u32 = 9;
pub const JOIN_LOAD: u32 = 10;
pub const JOIN_CARRY: u32 = 11;
pub const JOIN_NEXT_PIECE: u32 = 12;
pub const JOIN_FINAL_PASS: u32 = 13;
pub const JOIN_HOLD: u32 = 14;
pub const JOIN_TAIL: u32 = 15;

impl JoinEvent {
    fn event(&self) -> Result<Event> {
        let flag = match self.flag {
            0 => false,
            1 => true,
            flag => bail!("a join event's flag of {flag}"),
        };
        Ok(match self.kind {
            JOIN_INNER => Event::Inner {
                held: self.held,
                rows: self.rows,
                estimate: self.estimate,
                needed: self.memory,
            },
            JOIN_MADE => Event::Made,
            JOIN_FULL => Event::Full {
                side: match self.side {
                    0 => Side::Inner,
                    1 => Side::Outer,
                    side => bail!("a join's side {side}"),
                },
                partition: self.partition,
                tail: flag,
            },
            JOIN_GROWN => Event::Grown {
                memory: self.memory,
            },
            JOIN_SENT => Event::Sent {
                freed: flag,
                memory: self.memory,
            },
            JOIN_BUILD_END => Event::BuildEnd,
            JOIN_OUTER_END => Event::OuterEnd,
            JOIN_NEXT => Event::Next {
                memory: self.memory,
            },
            JOIN_TAKE => Event::Take {
                partition: self.partition,
                memory: self.memory,
            },
            JOIN_PIECE => Event::Piece {
                memory: self.memory,
            },
            JOIN_BLOCK => Event::Block {
                values: flag,
                len: self.len,
            },
            JOIN_FILE_END => Event::FileEnd,
            JOIN_PASS_END => Event::PassEnd,
            JOIN_TABLE_END => Event::TableEnd { held: flag },
            JOIN_TAIL_END => Event::TailEnd,
            kind => bail!("a join event of kind {kind}"),
        })
    }
}

impl JoinAction {
    fn start(start: &Start) -> Self {
        let plan = &start.plan;
        Self {
            kind: JOIN_START,
            outer_len: start.outer_len,
            plan: SpillPlan {
                chunk_len: plan.chunk_len,
                words: plan.words,
                partitions: plan.partitions,
                shift: plan.shift,
                below: plan.below,
                deeper: plan.deeper,
            },
            ..Self::default()
        }
    }

    fn of(action: Action) -> Self {
        let (kind, partition) = match action {
            Action::Start(start) => return Self::start(&start),
            Action::Done => (JOIN_DONE, 0),
            Action::AddChunk => (JOIN_ADD_CHUNK, 0),
            Action::Flush => (JOIN_FLUSH, 0),
            Action::Demote(partition) => (JOIN_DEMOTE, partition),
            Action::Evict(partition) => (JOIN_EVICT, partition),
            Action::Release(partition) => (JOIN_RELEASE, partition),
            Action::Open { partition, pieces } => {
                return Self {
                    kind: JOIN_OPEN,
                    partition,
                    pieces: u32::from(pieces),
                    ..Self::default()
                };
            }
            Action::LevelDone => (JOIN_LEVEL_DONE, 0),
            Action::Load => (JOIN_LOAD, 0),
            Action::Carry => (JOIN_CARRY, 0),
            Action::NextPiece => (JOIN_NEXT_PIECE, 0),
            Action::FinalPass => (JOIN_FINAL_PASS, 0),
            Action::Hold => (JOIN_HOLD, 0),
            Action::Tail => (JOIN_TAIL, 0),
            Action::Split(below) => {
                return Self {
                    kind: JOIN_SPLIT,
                    partition: below.partition,
                    below: JoinBelow {
                        partition: below.partition,
                        level: below.level,
                        expected: below.expected,
                        limit: below.limit,
                        shift: below.shift,
                        spare: 0,
                    },
                    ..Self::default()
                };
            }
        };
        Self {
            kind,
            partition,
            ..Self::default()
        }
    }
}

/// The machine at `machine`, checked for its alignment.
///
/// # Safety
///
/// `machine` must be null or point to a machine this process alone uses.
unsafe fn machine_at<'a>(machine: *mut Join) -> Result<&'a mut Join> {
    ensure!(machine.is_aligned(), "a join's machine not aligned to 8");
    // SAFETY: the caller's contract; any bytes are a machine, whose steps
    // check them.
    unsafe { machine.as_mut() }.context("a null join's machine")
}

/// `tess_join_init`: make `*machine` the machine of a join whose table
/// fits so far, for `params`.
///
/// # Safety
///
/// `machine` as for [`machine_at`]; `params` null or readable; `status`
/// as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_join_init(
    machine: *mut Join,
    params: *const JoinParams,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let machine = machine_at(machine)?;
            let params = params.as_ref().context("null join parameters")?;
            *machine = Join::new(&params.params())?;
            Ok(())
        })
    }
}

/// `tess_join_below`: make `*machine` the machine of the level below
/// that a `TESS_JOIN_SPLIT` asked for, and the level to make into
/// `*action`.
///
/// # Safety
///
/// As for [`tess_join_init`]; `below` null or readable; `action` null or
/// writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_join_below(
    machine: *mut Join,
    params: *const JoinParams,
    below: *const JoinBelow,
    action: *mut JoinAction,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let machine = machine_at(machine)?;
            let params = params.as_ref().context("null join parameters")?;
            let below = below.as_ref().context("a null level below")?.below()?;
            let out = action.as_mut().context("a null join action")?;
            let (join, start) = Join::below(&params.params(), &below)?;
            *machine = join;
            *out = JoinAction::start(&start);
            Ok(())
        })
    }
}

/// `tess_join_given`: make `*machine` the machine of the level of a
/// build several processes share, whose `partitions` one of them
/// planned, and that level into `*action`, a `TESS_JOIN_START`.
///
/// # Safety
///
/// As for [`tess_join_init`]; `action` null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_join_given(
    machine: *mut Join,
    params: *const JoinParams,
    partitions: u32,
    action: *mut JoinAction,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let machine = machine_at(machine)?;
            let params = params.as_ref().context("null join parameters")?;
            let out = action.as_mut().context("a null join action")?;
            let (join, start) = Join::given(&params.params(), partitions)?;
            *machine = join;
            *out = JoinAction::start(&start);
            Ok(())
        })
    }
}

/// `tess_join_step`: give `*machine` the join's `event`, with the regions
/// of `host`, and its next action into `*action`.
///
/// # Safety
///
/// `machine` as for [`machine_at`]; `host` null or readable, its words
/// refused as [`check_spill_words`] says or valid for reads and writes of
/// `nwords` words this thread alone reaches, its `rows`, `outer` and
/// `disk` null or valid for reads of their counts of words, and its
/// `routes` null or valid for writes of `nroutes` bytes, none of them
/// overlapping the words or the routes; `event` null or readable;
/// `action` null or writable; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_join_step(
    machine: *mut Join,
    host: *const JoinHost,
    event: *const JoinEvent,
    action: *mut JoinAction,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let machine = machine_at(machine)?;
            let host = host.as_ref().context("a null join host")?;
            let event = event.as_ref().context("a null join event")?.event()?;
            let out = action.as_mut().context("a null join action")?;
            let words: &[Cell<u64>] = if host.nwords == 0 {
                &[]
            } else {
                check_spill_words(host.words, host.nwords)?;
                // SAFETY: the caller's contract; a `Cell<u64>` has the
                // size and alignment of a `u64`.
                std::slice::from_raw_parts(host.words.cast::<Cell<u64>>(), host.nwords)
            };
            let rows = values(host.rows, host.nrows, "rows of a join's partitions")?;
            let outer = values(host.outer, host.nouter, "outer rows of a join's partitions")?;
            let disk = values(
                host.disk,
                host.ndisk,
                "bytes on disk of a join's partitions",
            )?;
            let routes = slots(host.routes, host.nroutes, "routes of a join's outer rows")?;
            let mut host = Host {
                words,
                rows,
                total: host.total,
                outer,
                disk,
                routes,
            };
            *out = JoinAction::of(machine.step(event, &mut host)?);
            Ok(())
        })
    }
}

/// `TessJoinLayout`: what [`tess_join_layout`] gives the size or offset
/// of.
pub const JOIN_MACHINE_SIZE: c_int = 1;
pub const JOIN_PARAMS_SIZE: c_int = 2;
pub const JOIN_PARAMS_TAIL_OFFSET: c_int = 3;
pub const JOIN_HOST_SIZE: c_int = 4;
pub const JOIN_HOST_NROUTES_OFFSET: c_int = 5;
pub const JOIN_EVENT_SIZE: c_int = 6;
pub const JOIN_EVENT_MEMORY_OFFSET: c_int = 7;
pub const JOIN_ACTION_SIZE: c_int = 8;
pub const JOIN_ACTION_PLAN_OFFSET: c_int = 9;
pub const JOIN_ACTION_BELOW_OFFSET: c_int = 10;
pub const JOIN_BELOW_SIZE: c_int = 11;
pub const JOIN_BELOW_SHIFT_OFFSET: c_int = 12;
pub const JOIN_EVENT_LEN_OFFSET: c_int = 13;
pub const JOIN_ACTION_OUTER_LEN_OFFSET: c_int = 14;

/// `tess_join_layout`: the size or offset of a `TessJoinLayout` for the C
/// side's mirrors to check; 0 for another code.
#[unsafe(no_mangle)]
pub extern "C" fn tess_join_layout(what: c_int) -> usize {
    match what {
        JOIN_MACHINE_SIZE => size_of::<Join>(),
        JOIN_PARAMS_SIZE => size_of::<JoinParams>(),
        JOIN_PARAMS_TAIL_OFFSET => offset_of!(JoinParams, tail),
        JOIN_HOST_SIZE => size_of::<JoinHost>(),
        JOIN_HOST_NROUTES_OFFSET => offset_of!(JoinHost, nroutes),
        JOIN_EVENT_SIZE => size_of::<JoinEvent>(),
        JOIN_EVENT_MEMORY_OFFSET => offset_of!(JoinEvent, memory),
        JOIN_ACTION_SIZE => size_of::<JoinAction>(),
        JOIN_ACTION_PLAN_OFFSET => offset_of!(JoinAction, plan),
        JOIN_ACTION_BELOW_OFFSET => offset_of!(JoinAction, below),
        JOIN_BELOW_SIZE => size_of::<JoinBelow>(),
        JOIN_BELOW_SHIFT_OFFSET => offset_of!(JoinBelow, shift),
        JOIN_EVENT_LEN_OFFSET => offset_of!(JoinEvent, len),
        JOIN_ACTION_OUTER_LEN_OFFSET => offset_of!(JoinAction, outer_len),
        _ => 0,
    }
}
