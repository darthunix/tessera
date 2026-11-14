//! The machine of a grouping's spill ([`relspill::group`]), declared in
//! `include/tessera/spill.h`. The grouping keeps a machine for each level
//! of its spill, and the regions a step reads, and steps it with what its
//! table saw.

use std::cell::Cell;
use std::ffi::c_int;
use std::mem::{offset_of, size_of};

use anyhow::{Context, Result, bail, ensure};
use relspill::group::{Action, Below, Event, Group, Host, Params, Rows, Table};
use relspill::plan::{Plan, Shape};

use super::shared_spill::check_spill_words;
use super::spill::{SpillPlan, SpillShape};
use super::status::{Code, Status, guard};
use super::table::{output, values};

/// `TessGroupParams`: what every level of a grouping's spill shares; see
/// [`Params`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct GroupParams {
    pub limit: u64,
    pub shape: SpillShape,
    pub merged: u64,
    pub fill: u64,
    pub start: f64,
    pub target: f64,
    pub spilled: f64,
    pub split_room: f64,
}

impl GroupParams {
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
            merged: self.merged,
            fill: self.fill,
            start: self.start,
            target: self.target,
            spilled: self.spilled,
            split_room: self.split_room,
        }
    }
}

/// `TessGroupHost`: the regions of a level a step reads; see [`Host`].
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct GroupHost {
    pub words: *mut u64,
    pub nwords: usize,
    pub records: *const u64,
    pub nrecords: usize,
    pub disk: *const u64,
    pub ndisk: usize,
}

/// `TessGroupEvent`: a step of the grouping; see [`Event`]. `partition`
/// and `flag`, the tail, are those of [`GROUP_FULL`], `flag` the force of
/// [`GROUP_INNER`]; `bytes`, `records` and `estimate` those of
/// [`GROUP_INNER`]; `memory` that of [`GROUP_GROWN`] and [`GROUP_SENT`];
/// `groups`, `others` and `memory` those of [`GROUP_WEIGHED`]; `memory`,
/// `records`, `rows` and `flag`, whether the groups can spill, those of
/// [`GROUP_PARTIAL`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct GroupEvent {
    pub kind: u32,
    pub partition: u32,
    pub flag: u32,
    pub spare: u32,
    pub bytes: u64,
    pub records: u64,
    pub estimate: f64,
    pub memory: u64,
    pub groups: u64,
    pub others: u64,
    pub rows: u64,
}

/// `TessGroupEventKind`, of a [`GroupEvent`].
pub const GROUP_INNER: u32 = 1;
pub const GROUP_MADE: u32 = 2;
pub const GROUP_FULL: u32 = 3;
pub const GROUP_GROWN: u32 = 4;
pub const GROUP_SENT: u32 = 5;
pub const GROUP_INPUT_END: u32 = 6;
pub const GROUP_NEXT: u32 = 7;
pub const GROUP_WEIGHED: u32 = 8;
pub const GROUP_PARTIAL: u32 = 9;

/// `TessGroupBelow`: a level below to make; see [`Below`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GroupBelow {
    pub partition: u32,
    pub level: u32,
    pub expected: u64,
    pub limit: u64,
    pub shift: u32,
    pub spare: u32,
}

/// `TessGroupAction`: what the grouping does next; see [`Action`]. The
/// plan is that of [`GROUP_START`], `partition` that of the actions on a
/// partition, and `below` that of [`GROUP_SPLIT`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GroupAction {
    pub kind: u32,
    pub partition: u32,
    pub plan: SpillPlan,
    pub below: GroupBelow,
}

/// `TessGroupActionKind`, of a [`GroupAction`].
pub const GROUP_DONE: u32 = 0;
pub const GROUP_START: u32 = 1;
pub const GROUP_ADD_CHUNK: u32 = 2;
pub const GROUP_FLUSH: u32 = 3;
pub const GROUP_WRITE: u32 = 4;
pub const GROUP_MERGE: u32 = 5;
pub const GROUP_WEIGH: u32 = 6;
pub const GROUP_SPLIT: u32 = 7;
pub const GROUP_LEVEL_DONE: u32 = 8;
pub const GROUP_EMIT: u32 = 9;
pub const GROUP_SPILL: u32 = 10;

impl GroupEvent {
    fn event(&self) -> Result<Event> {
        let flag = match self.flag {
            0 => false,
            1 => true,
            flag => bail!("a grouping event's flag of {flag}"),
        };
        ensure!(
            self.spare == 0,
            "a grouping event with a spare word of {}",
            self.spare
        );
        Ok(match self.kind {
            GROUP_INNER => Event::Inner {
                bytes: self.bytes,
                records: self.records,
                estimate: self.estimate,
                force: flag,
            },
            GROUP_MADE => Event::Made,
            GROUP_FULL => Event::Full {
                partition: self.partition,
                tail: flag,
            },
            GROUP_GROWN => Event::Grown {
                memory: self.memory,
            },
            GROUP_SENT => Event::Sent {
                memory: self.memory,
            },
            GROUP_INPUT_END => Event::InputEnd,
            GROUP_NEXT => Event::Next,
            GROUP_WEIGHED => Event::Weighed {
                groups: self.groups,
                others: self.others,
                memory: self.memory,
            },
            GROUP_PARTIAL => Event::Partial {
                memory: self.memory,
                records: self.records,
                rows: self.rows,
                spills: flag,
            },
            kind => bail!("a grouping event of kind {kind}"),
        })
    }
}

fn spill_plan(plan: &Plan) -> SpillPlan {
    SpillPlan {
        chunk_len: plan.chunk_len,
        words: plan.words,
        partitions: plan.partitions,
        shift: plan.shift,
        below: plan.below,
        deeper: plan.deeper,
    }
}

impl GroupAction {
    fn start(plan: &Plan) -> Self {
        Self {
            kind: GROUP_START,
            plan: spill_plan(plan),
            ..Self::default()
        }
    }

    fn of(action: Action) -> Self {
        let (kind, partition) = match action {
            Action::Start(plan) => return Self::start(&plan),
            Action::Split(below) => {
                return Self {
                    kind: GROUP_SPLIT,
                    partition: below.partition,
                    below: GroupBelow {
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
            Action::Done => (GROUP_DONE, 0),
            Action::AddChunk => (GROUP_ADD_CHUNK, 0),
            Action::Flush => (GROUP_FLUSH, 0),
            Action::Write(partition) => (GROUP_WRITE, partition),
            Action::Merge(partition) => (GROUP_MERGE, partition),
            Action::Weigh(partition) => (GROUP_WEIGH, partition),
            Action::LevelDone => (GROUP_LEVEL_DONE, 0),
            Action::Emit => (GROUP_EMIT, 0),
            Action::Spill => (GROUP_SPILL, 0),
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
unsafe fn machine_at<'a>(machine: *mut Group) -> Result<&'a mut Group> {
    ensure!(
        machine.is_aligned(),
        "a grouping's machine not aligned to 8"
    );
    // SAFETY: the caller's contract; any bytes are a machine, whose steps
    // check them.
    unsafe { machine.as_mut() }.context("a null grouping's machine")
}

/// `tess_group_init`: make `*machine` the machine of a grouping whose
/// table fits so far, for `params`.
///
/// # Safety
///
/// `machine` null or valid for a write of a machine, whatever it holds
/// before; `params` null or readable; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_group_init(
    machine: *mut Group,
    params: *const GroupParams,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let machine = output(machine, "grouping's machine")?;
            let params = params.as_ref().context("null grouping parameters")?;
            machine.write(Group::new(&params.params())?);
            Ok(())
        })
    }
}

/// `tess_group_below`: make `*machine` the machine of the level below
/// that a `TESS_GROUP_SPLIT` asked for, and its plan into `*action`.
///
/// # Safety
///
/// As for [`tess_group_init`]; `below` null or readable; `action` null or
/// valid for a write, whatever it holds before.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_group_below(
    machine: *mut Group,
    params: *const GroupParams,
    below: *const GroupBelow,
    action: *mut GroupAction,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let machine = output(machine, "grouping's machine")?;
            let params = params.as_ref().context("null grouping parameters")?;
            let below = below.as_ref().context("a null level below")?;
            ensure!(
                below.spare == 0,
                "a level below with a spare word of {}",
                below.spare
            );
            let out = output(action, "grouping action")?;
            let (group, plan) = Group::below(
                &params.params(),
                &Below {
                    partition: below.partition,
                    level: below.level,
                    expected: below.expected,
                    limit: below.limit,
                    shift: below.shift,
                },
            )?;
            machine.write(group);
            out.write(GroupAction::start(&plan));
            Ok(())
        })
    }
}

/// `tess_group_step`: give `*machine` the grouping's `event`, with the
/// regions of `host`, and its next action into `*action`.
///
/// # Safety
///
/// `machine` as for [`machine_at`]; `host` null or readable, its words
/// refused as [`check_spill_words`] says or valid for reads and writes of
/// `nwords` words this thread alone reaches, its `records` and `disk`
/// null or valid for reads of their counts of words, not overlapping the
/// words; `event` null or readable; `action` null or valid for a write,
/// whatever it holds before; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_group_step(
    machine: *mut Group,
    host: *const GroupHost,
    event: *const GroupEvent,
    action: *mut GroupAction,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let machine = machine_at(machine)?;
            let host = host.as_ref().context("a null grouping host")?;
            let event = event.as_ref().context("a null grouping event")?.event()?;
            let out = output(action, "grouping action")?;
            let words: &[Cell<u64>] = if host.nwords == 0 {
                &[]
            } else {
                check_spill_words(host.words, host.nwords)?;
                // SAFETY: the caller's contract; a `Cell<u64>` has the
                // size and alignment of a `u64`.
                std::slice::from_raw_parts(host.words.cast::<Cell<u64>>(), host.nwords)
            };
            let records = values(
                host.records,
                host.nrecords,
                "records of a grouping's partitions",
            )?;
            let disk = values(
                host.disk,
                host.ndisk,
                "bytes on disk of a grouping's partitions",
            )?;
            let host = Host {
                words,
                records,
                disk,
            };
            out.write(GroupAction::of(machine.step(event, &host)?));
            Ok(())
        })
    }
}

/// `TessGroupLayout`: what [`tess_group_layout`] gives the size or offset
/// of.
pub const GROUP_MACHINE_SIZE: c_int = 1;
pub const GROUP_PARAMS_SIZE: c_int = 2;
pub const GROUP_PARAMS_SPLIT_ROOM_OFFSET: c_int = 3;
pub const GROUP_HOST_SIZE: c_int = 4;
pub const GROUP_HOST_NDISK_OFFSET: c_int = 5;
pub const GROUP_EVENT_SIZE: c_int = 6;
pub const GROUP_EVENT_ROWS_OFFSET: c_int = 7;
pub const GROUP_ACTION_SIZE: c_int = 8;
pub const GROUP_ACTION_BELOW_OFFSET: c_int = 9;
pub const GROUP_BELOW_SIZE: c_int = 10;
pub const GROUP_BELOW_SHIFT_OFFSET: c_int = 11;

/// `tess_group_layout`: the size or offset of a `TessGroupLayout` for
/// the C side's mirrors to check; 0 for another code.
#[unsafe(no_mangle)]
pub extern "C" fn tess_group_layout(what: c_int) -> usize {
    match what {
        GROUP_MACHINE_SIZE => size_of::<Group>(),
        GROUP_PARAMS_SIZE => size_of::<GroupParams>(),
        GROUP_PARAMS_SPLIT_ROOM_OFFSET => offset_of!(GroupParams, split_room),
        GROUP_HOST_SIZE => size_of::<GroupHost>(),
        GROUP_HOST_NDISK_OFFSET => offset_of!(GroupHost, ndisk),
        GROUP_EVENT_SIZE => size_of::<GroupEvent>(),
        GROUP_EVENT_ROWS_OFFSET => offset_of!(GroupEvent, rows),
        GROUP_ACTION_SIZE => size_of::<GroupAction>(),
        GROUP_ACTION_BELOW_OFFSET => offset_of!(GroupAction, below),
        GROUP_BELOW_SIZE => size_of::<GroupBelow>(),
        GROUP_BELOW_SHIFT_OFFSET => offset_of!(GroupBelow, shift),
        _ => 0,
    }
}

/// `TessRowsParams`: what every level of a grouping's rows shares; see
/// [`RowsParams`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct RowsParams {
    pub limit: u64,
    pub shape: SpillShape,
    pub fill: u64,
    pub least: u64,
}

impl RowsParams {
    fn params(&self) -> relspill::group::RowsParams {
        let shape = &self.shape;
        relspill::group::RowsParams {
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
            fill: self.fill,
            least: self.least,
        }
    }
}

/// `TessRowsHost`: the rows written of each partition of a level of
/// rows; see [`RowsHost`].
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct RowsHost {
    pub written: *const u64,
    pub nwritten: usize,
}

/// `TessRowsEvent`: a step of a grouping's rows; see [`RowsEvent`].
/// `memory` and `records` are those of [`ROWS_GROWN`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct RowsEvent {
    pub kind: u32,
    pub spare: u32,
    pub memory: u64,
    pub records: u64,
}

/// `TessRowsEventKind`, of a [`RowsEvent`].
pub const ROWS_GROWN: u32 = 1;
pub const ROWS_WRITTEN: u32 = 2;
pub const ROWS_NEXT: u32 = 3;

/// `TessRowsTable`: a table of a partition of rows read back; see
/// [`Table`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RowsTable {
    pub partition: u32,
    pub level: u32,
    pub shift: u32,
    pub last: u32,
}

/// `TessRowsAction`: what a grouping's rows do next; see [`RowsAction`].
/// The plan and `level` are those of [`ROWS_FREEZE`], `table` that of
/// [`ROWS_READ`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RowsAction {
    pub kind: u32,
    pub level: u32,
    pub plan: SpillPlan,
    pub table: RowsTable,
}

/// `TessRowsActionKind`, of a [`RowsAction`].
pub const ROWS_DONE: u32 = 0;
pub const ROWS_FREEZE: u32 = 1;
pub const ROWS_READ: u32 = 2;
pub const ROWS_LEVEL_DONE: u32 = 3;

impl RowsAction {
    fn of(action: relspill::group::RowsAction) -> Self {
        use relspill::group::RowsAction as Rust;
        match action {
            Rust::Done => Self::default(),
            Rust::Freeze { plan, level } => Self {
                kind: ROWS_FREEZE,
                level,
                plan: spill_plan(&plan),
                ..Self::default()
            },
            Rust::Read(table) => Self {
                kind: ROWS_READ,
                table: RowsTable {
                    partition: table.partition,
                    level: table.level,
                    shift: table.shift,
                    last: u32::from(table.last),
                },
                ..Self::default()
            },
            Rust::LevelDone => Self {
                kind: ROWS_LEVEL_DONE,
                ..Self::default()
            },
        }
    }
}

/// `tess_rows_init`: make `*machine` the machine of the table of a
/// grouping's input, whose rows go to the first level once frozen.
///
/// # Safety
///
/// `machine` null or valid for a write of a machine, whatever it holds
/// before; `params` null or readable; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_rows_init(
    machine: *mut Rows,
    params: *const RowsParams,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let machine = output(machine, "rows' machine")?;
            let params = params.as_ref().context("null rows parameters")?;
            machine.write(Rows::new(&params.params())?);
            Ok(())
        })
    }
}

/// `tess_rows_read`: make `*machine` the machine of the table of the
/// partition a `TESS_ROWS_READ` gave.
///
/// # Safety
///
/// As for [`tess_rows_init`]; `table` null or readable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_rows_read(
    machine: *mut Rows,
    params: *const RowsParams,
    table: *const RowsTable,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let machine = output(machine, "rows' machine")?;
            let params = params.as_ref().context("null rows parameters")?;
            let table = table.as_ref().context("a null table of rows")?;
            let last = match table.last {
                0 => false,
                1 => true,
                last => bail!("a table of rows whose last is {last}"),
            };
            let table = Table {
                partition: table.partition,
                level: table.level,
                shift: table.shift,
                last,
            };
            machine.write(Rows::read(&params.params(), &table)?);
            Ok(())
        })
    }
}

/// `tess_rows_step`: give `*machine` the grouping's `event` of its rows,
/// with the regions of `host`, and its next action into `*action`.
///
/// # Safety
///
/// `machine` null or a machine this process alone uses; `host` null or
/// readable, its `written` null or valid for reads of `nwritten` words;
/// `event` null or readable; `action` null or valid for a write, whatever
/// it holds before; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_rows_step(
    machine: *mut Rows,
    host: *const RowsHost,
    event: *const RowsEvent,
    action: *mut RowsAction,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            ensure!(machine.is_aligned(), "a rows' machine not aligned to 8");
            // SAFETY: any bytes are a machine, whose steps check them.
            let machine = machine.as_mut().context("a null rows' machine")?;
            let host = host.as_ref().context("a null rows host")?;
            let event = event.as_ref().context("a null rows event")?;
            ensure!(
                event.spare == 0,
                "a rows event with a spare word of {}",
                event.spare
            );
            let event = match event.kind {
                ROWS_GROWN => relspill::group::RowsEvent::Grown {
                    memory: event.memory,
                    records: event.records,
                },
                ROWS_WRITTEN => relspill::group::RowsEvent::Written,
                ROWS_NEXT => relspill::group::RowsEvent::Next,
                kind => bail!("a rows event of kind {kind}"),
            };
            let out = output(action, "rows action")?;
            let written = values(
                host.written,
                host.nwritten,
                "rows written of the partitions",
            )?;
            let host = relspill::group::RowsHost { written };
            out.write(RowsAction::of(machine.step(event, &host)?));
            Ok(())
        })
    }
}

/// `TessRowsLayout`: what [`tess_rows_layout`] gives the size or offset
/// of.
pub const ROWS_MACHINE_SIZE: c_int = 1;
pub const ROWS_PARAMS_SIZE: c_int = 2;
pub const ROWS_PARAMS_LEAST_OFFSET: c_int = 3;
pub const ROWS_HOST_SIZE: c_int = 4;
pub const ROWS_EVENT_SIZE: c_int = 5;
pub const ROWS_ACTION_SIZE: c_int = 6;
pub const ROWS_ACTION_TABLE_OFFSET: c_int = 7;

/// `tess_rows_layout`: the size or offset of a `TessRowsLayout` for the
/// C side's mirrors to check; 0 for another code.
#[unsafe(no_mangle)]
pub extern "C" fn tess_rows_layout(what: c_int) -> usize {
    match what {
        ROWS_MACHINE_SIZE => size_of::<Rows>(),
        ROWS_PARAMS_SIZE => size_of::<RowsParams>(),
        ROWS_PARAMS_LEAST_OFFSET => offset_of!(RowsParams, least),
        ROWS_HOST_SIZE => size_of::<RowsHost>(),
        ROWS_EVENT_SIZE => size_of::<RowsEvent>(),
        ROWS_ACTION_SIZE => size_of::<RowsAction>(),
        ROWS_ACTION_TABLE_OFFSET => offset_of!(RowsAction, table),
        _ => 0,
    }
}
