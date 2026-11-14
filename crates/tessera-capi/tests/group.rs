//! The machine of a grouping's spill through its C entry points: a spill
//! step by step, a level below, the refusals and the layout of the
//! mirrors.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test reports a failure by panicking"
)]

use std::mem::{offset_of, size_of};

use tessera_capi::c::{
    Code, GROUP_ACTION_BELOW_OFFSET, GROUP_ACTION_SIZE, GROUP_ADD_CHUNK, GROUP_BELOW_SHIFT_OFFSET,
    GROUP_BELOW_SIZE, GROUP_DONE, GROUP_EMIT, GROUP_EVENT_ROWS_OFFSET, GROUP_EVENT_SIZE,
    GROUP_FLUSH, GROUP_FULL, GROUP_GROWN, GROUP_HOST_NDISK_OFFSET, GROUP_HOST_SIZE, GROUP_INNER,
    GROUP_INPUT_END, GROUP_LEVEL_DONE, GROUP_MACHINE_SIZE, GROUP_MADE, GROUP_MERGE, GROUP_NEXT,
    GROUP_PARAMS_SIZE, GROUP_PARAMS_SPLIT_ROOM_OFFSET, GROUP_PARTIAL, GROUP_SENT, GROUP_SPILL,
    GROUP_SPLIT, GROUP_START, GROUP_WEIGH, GROUP_WEIGHED, GROUP_WRITE, GroupAction, GroupBelow,
    GroupEvent, GroupHost, GroupParams, ROWS_ACTION_SIZE, ROWS_ACTION_TABLE_OFFSET, ROWS_DONE,
    ROWS_EVENT_SIZE, ROWS_FREEZE, ROWS_GROWN, ROWS_HOST_SIZE, ROWS_LEVEL_DONE, ROWS_MACHINE_SIZE,
    ROWS_NEXT, ROWS_PARAMS_LEAST_OFFSET, ROWS_PARAMS_SIZE, ROWS_READ, ROWS_WRITTEN, RowsAction,
    RowsEvent, RowsHost, RowsParams, RowsTable, SpillLevel, SpillPlan, SpillShape, Status,
    tess_group_below, tess_group_init, tess_group_layout, tess_group_step, tess_rows_init,
    tess_rows_layout, tess_rows_read, tess_rows_step, tess_spill_expected, tess_spill_plan,
    tess_table_spill_add_bytes,
};

/// The words of a machine.
type Machine = [u64; 20];

/// The words of a machine of rows.
type RowsMachine = [u64; 13];

fn params(limit: u64) -> GroupParams {
    GroupParams {
        limit,
        shape: SpillShape {
            reserve: 1024 + 512,
            share: 8,
            min_chunk: 1024,
            max_chunk: 64 * 1024,
            record: 32,
            min_partitions: 4,
            max_partitions: 64,
        },
        merged: 48,
        fill: 7,
        start: 0.875,
        target: 0.5,
        spilled: 1.0,
        split_room: 1.0,
    }
}

/// A level's regions as C keeps them.
struct Level {
    words: Vec<u64>,
    records: Vec<u64>,
    disk: Vec<u64>,
}

impl Level {
    fn none() -> Self {
        Self {
            words: Vec::new(),
            records: Vec::new(),
            disk: Vec::new(),
        }
    }

    fn of(plan: &SpillPlan) -> Self {
        let partitions = plan.partitions as usize;
        Self {
            words: vec![0; plan.words],
            records: vec![0; partitions],
            disk: vec![0; partitions],
        }
    }

    fn host(&mut self) -> GroupHost {
        GroupHost {
            words: self.words.as_mut_ptr(),
            nwords: self.words.len(),
            records: self.records.as_ptr(),
            nrecords: self.records.len(),
            disk: self.disk.as_ptr(),
            ndisk: self.disk.len(),
        }
    }

    /// Count bytes of a partition's chunks in memory.
    fn add(&mut self, partition: i32, bytes: i64) {
        let mut status = Status::new();
        let mut over = false;
        // SAFETY: the level's words, a local flag and status.
        let code = unsafe {
            tess_table_spill_add_bytes(
                self.words.as_mut_ptr(),
                self.words.len(),
                false,
                bytes,
                partition,
                &raw mut over,
                &raw mut status,
            )
        };
        assert_eq!(code, Code::Ok, "{}", status.message());
    }
}

fn step(machine: &mut Machine, level: &mut Level, event: GroupEvent) -> (Code, GroupAction) {
    let mut status = Status::new();
    let mut action = GroupAction::default();
    let host = level.host();
    // SAFETY: a local machine, regions, event, action and status.
    let code = unsafe {
        tess_group_step(
            machine.as_mut_ptr().cast(),
            &raw const host,
            &raw const event,
            &raw mut action,
            &raw mut status,
        )
    };
    (code, action)
}

fn event(kind: u32) -> GroupEvent {
    GroupEvent {
        kind,
        ..GroupEvent::default()
    }
}

fn kind(kind: u32) -> GroupAction {
    GroupAction {
        kind,
        ..GroupAction::default()
    }
}

fn on(kind: u32, partition: u32) -> GroupAction {
    GroupAction {
        partition,
        ..self::kind(kind)
    }
}

fn init(machine: &mut Machine, params: &GroupParams) -> Code {
    let mut status = Status::new();
    // SAFETY: a local machine, parameters and status.
    unsafe { tess_group_init(machine.as_mut_ptr().cast(), params, &raw mut status) }
}

#[test]
fn a_grouping_spills_step_by_step_by_the_entry_points() {
    let limit = 1 << 20;
    let params = params(limit);
    let mut machine: Machine = [0; 20];
    assert_eq!(init(&mut machine, &params), Code::Ok);
    let mut none = Level::none();
    let inner = |bytes, flag| GroupEvent {
        bytes,
        records: 3000,
        estimate: 30_000.0,
        flag,
        ..event(GROUP_INNER)
    };
    assert_eq!(
        step(&mut machine, &mut none, inner(900_000, 0)),
        (Code::Ok, kind(GROUP_DONE))
    );
    let (code, start) = step(&mut machine, &mut none, inner(950_000, 0));
    assert_eq!((code, start.kind), (Code::Ok, GROUP_START));
    let mut status = Status::new();
    let mut plan = SpillPlan::default();
    let level = SpillLevel {
        expected: tess_spill_expected(950_000.0, 3000.0, 30_000.0, 1),
        limit: limit as usize,
        ..SpillLevel::default()
    };
    // SAFETY: local arguments.
    let code = unsafe {
        tess_spill_plan(
            &raw const params.shape,
            &raw const level,
            &raw mut plan,
            &raw mut status,
        )
    };
    assert_eq!((code, start.plan), (Code::Ok, plan));
    let mut level = Level::of(&start.plan);
    assert_eq!(
        step(&mut machine, &mut level, event(GROUP_MADE)),
        (Code::Ok, kind(GROUP_DONE))
    );
    let full = GroupEvent {
        partition: 2,
        flag: 1,
        ..event(GROUP_FULL)
    };
    assert_eq!(
        step(&mut machine, &mut level, full),
        (Code::Ok, kind(GROUP_ADD_CHUNK))
    );
    // The largest goes past seven eighths, down to half.
    level.add(2, 600_000);
    level.add(1, 300_000);
    let grown = GroupEvent {
        memory: 950_000,
        ..event(GROUP_GROWN)
    };
    assert_eq!(
        step(&mut machine, &mut level, grown),
        (Code::Ok, on(GROUP_WRITE, 2))
    );
    level.add(2, -600_000);
    let sent = GroupEvent {
        memory: 350_000,
        ..event(GROUP_SENT)
    };
    assert_eq!(
        step(&mut machine, &mut level, sent),
        (Code::Ok, kind(GROUP_DONE))
    );
    // The input ends: partition 2 has records on disk and none in memory.
    level.disk[2] = 600_000;
    level.records[1] = 100;
    assert_eq!(
        step(&mut machine, &mut level, event(GROUP_INPUT_END)),
        (Code::Ok, kind(GROUP_DONE))
    );
    assert_eq!(
        step(&mut machine, &mut level, event(GROUP_NEXT)),
        (Code::Ok, on(GROUP_MERGE, 1))
    );
    level.records[1] = 0;
    assert_eq!(
        step(&mut machine, &mut level, event(GROUP_NEXT)),
        (Code::Ok, on(GROUP_WEIGH, 2))
    );
    let weighed = GroupEvent {
        groups: 100_000,
        others: 0,
        ..event(GROUP_WEIGHED)
    };
    let (code, split) = step(&mut machine, &mut level, weighed);
    let below = GroupBelow {
        partition: 2,
        level: 1,
        expected: 600_000,
        shift: start.plan.below,
        spare: 0,
    };
    assert_eq!(
        (code, split.kind, split.below),
        (Code::Ok, GROUP_SPLIT, below)
    );
    assert_eq!(
        step(&mut machine, &mut level, event(GROUP_NEXT)),
        (Code::Ok, kind(GROUP_LEVEL_DONE))
    );
    // The level below writes its partitions as they fill.
    let mut child: Machine = [0; 20];
    let mut action = GroupAction::default();
    // SAFETY: local arguments.
    let code = unsafe {
        tess_group_below(
            child.as_mut_ptr().cast(),
            &raw const params,
            &raw const below,
            &raw mut action,
            &raw mut status,
        )
    };
    assert_eq!(
        (code, action.kind, action.plan.shift),
        (Code::Ok, GROUP_START, below.shift)
    );
    let mut level = Level::of(&action.plan);
    assert_eq!(
        step(&mut child, &mut level, event(GROUP_MADE)),
        (Code::Ok, kind(GROUP_DONE))
    );
    assert_eq!(
        step(&mut child, &mut level, full),
        (Code::Ok, kind(GROUP_FLUSH))
    );
    let bad = GroupBelow { spare: 1, ..below };
    // SAFETY: local arguments.
    let code = unsafe {
        tess_group_below(
            child.as_mut_ptr().cast(),
            &raw const params,
            &raw const bad,
            &raw mut action,
            &raw mut status,
        )
    };
    assert_eq!(code, Code::InvalidArgument);
}

#[test]
fn a_partial_grouping_is_weighed_by_the_entry_points() {
    let mut machine = Machine::default();
    assert_eq!(init(&mut machine, &params(1 << 20)), Code::Ok);
    let mut none = Level::none();
    let partial = |records, flag| GroupEvent {
        memory: (1 << 20) / 8 * 7 + 1,
        records,
        rows: 2000,
        flag,
        ..event(GROUP_PARTIAL)
    };
    for (event, action) in [
        (partial(0, 1), GROUP_DONE),
        (partial(1000, 1), GROUP_EMIT),
        (partial(1001, 0), GROUP_EMIT),
        (partial(1001, 1), GROUP_SPILL),
    ] {
        assert_eq!(
            step(&mut machine, &mut none, event),
            (Code::Ok, kind(action))
        );
    }
}

#[test]
fn the_group_entry_points_refuse_what_is_no_step() {
    let mut machine: Machine = [0; 20];
    let mut none = Level::none();
    assert_eq!(
        step(&mut machine, &mut none, event(GROUP_NEXT)).0,
        Code::InvalidArgument
    );
    let bad = GroupParams {
        limit: 0,
        ..params(1 << 20)
    };
    assert_eq!(init(&mut machine, &bad), Code::InvalidArgument);
    assert_eq!(init(&mut machine, &params(1 << 20)), Code::Ok);
    let before = machine;
    for event in [
        event(GROUP_NEXT),
        event(99),
        GroupEvent {
            flag: 2,
            ..event(GROUP_INNER)
        },
        GroupEvent {
            spare: 1,
            ..event(GROUP_INNER)
        },
    ] {
        assert_eq!(
            step(&mut machine, &mut none, event).0,
            Code::InvalidArgument,
            "{event:?}"
        );
        assert_eq!(machine, before);
    }
    let mut status = Status::new();
    let mut action = GroupAction::default();
    let host = none.host();
    let inner = event(GROUP_INNER);
    // SAFETY: null and misaligned pointers, which the entry points refuse.
    unsafe {
        let words = machine.as_mut_ptr();
        let calls = [
            tess_group_step(
                std::ptr::null_mut(),
                &raw const host,
                &raw const inner,
                &raw mut action,
                &raw mut status,
            ),
            tess_group_step(
                words.cast::<u8>().add(1).cast(),
                &raw const host,
                &raw const inner,
                &raw mut action,
                &raw mut status,
            ),
            tess_group_step(
                words.cast(),
                std::ptr::null(),
                &raw const inner,
                &raw mut action,
                &raw mut status,
            ),
            tess_group_step(
                words.cast(),
                &raw const host,
                std::ptr::null(),
                &raw mut action,
                &raw mut status,
            ),
            tess_group_step(
                words.cast(),
                &raw const host,
                &raw const inner,
                std::ptr::null_mut(),
                &raw mut status,
            ),
            tess_group_init(words.cast(), std::ptr::null(), &raw mut status),
        ];
        assert!(
            calls.iter().all(|&code| code == Code::InvalidArgument),
            "{calls:?}"
        );
    }
}

#[test]
fn the_group_layout_is_its_types() {
    let sizes = [
        (GROUP_MACHINE_SIZE, size_of::<Machine>()),
        (GROUP_PARAMS_SIZE, size_of::<GroupParams>()),
        (
            GROUP_PARAMS_SPLIT_ROOM_OFFSET,
            offset_of!(GroupParams, split_room),
        ),
        (GROUP_HOST_SIZE, size_of::<GroupHost>()),
        (GROUP_HOST_NDISK_OFFSET, offset_of!(GroupHost, ndisk)),
        (GROUP_EVENT_SIZE, size_of::<GroupEvent>()),
        (GROUP_EVENT_ROWS_OFFSET, offset_of!(GroupEvent, rows)),
        (GROUP_ACTION_SIZE, size_of::<GroupAction>()),
        (GROUP_ACTION_BELOW_OFFSET, offset_of!(GroupAction, below)),
        (GROUP_BELOW_SIZE, size_of::<GroupBelow>()),
        (GROUP_BELOW_SHIFT_OFFSET, offset_of!(GroupBelow, shift)),
        (99, 0),
    ];
    for (what, size) in sizes {
        assert_eq!(tess_group_layout(what), size, "{what}");
    }
}

#[test]
fn the_group_entry_points_write_places_that_hold_nothing_yet() {
    use std::mem::MaybeUninit;

    let params = params(1 << 20);
    let mut machine = MaybeUninit::<Machine>::uninit();
    let mut action = MaybeUninit::<GroupAction>::uninit();
    let mut status = Status::new();
    // SAFETY: places valid for writes, which the entry points write whole.
    let code = unsafe {
        tess_group_init(
            machine.as_mut_ptr().cast(),
            &raw const params,
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok, "{}", status.message());
    let mut none = Level::none();
    let host = none.host();
    let inner = GroupEvent {
        bytes: 1 << 20,
        ..event(GROUP_INNER)
    };
    // SAFETY: the machine is made; the action a place valid for a write.
    let code = unsafe {
        tess_group_step(
            machine.as_mut_ptr().cast(),
            &raw const host,
            &raw const inner,
            action.as_mut_ptr(),
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok, "{}", status.message());
    // SAFETY: written whole by the step.
    assert_eq!(unsafe { action.assume_init() }.kind, GROUP_START);
    let below = GroupBelow {
        partition: 1,
        level: 1,
        expected: 1 << 20,
        shift: 2,
        spare: 0,
    };
    let mut other = MaybeUninit::<Machine>::uninit();
    // SAFETY: as above.
    let code = unsafe {
        tess_group_below(
            other.as_mut_ptr().cast(),
            &raw const params,
            &raw const below,
            action.as_mut_ptr(),
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok, "{}", status.message());
}

fn rows_params(limit: u64) -> RowsParams {
    RowsParams {
        limit,
        shape: SpillShape {
            reserve: 2048,
            share: 1,
            min_chunk: 1024,
            max_chunk: 1024,
            record: 0,
            min_partitions: 4,
            max_partitions: 4,
        },
        fill: 7,
        least: 64,
    }
}

fn rows_init(machine: &mut RowsMachine, params: &RowsParams) -> Code {
    let mut status = Status::new();
    // SAFETY: a local machine, parameters and status.
    unsafe { tess_rows_init(machine.as_mut_ptr().cast(), params, &raw mut status) }
}

fn rows_read(machine: &mut RowsMachine, params: &RowsParams, table: &RowsTable) -> Code {
    let mut status = Status::new();
    // SAFETY: a local machine, parameters, table and status.
    unsafe { tess_rows_read(machine.as_mut_ptr().cast(), params, table, &raw mut status) }
}

fn rows_step(machine: &mut RowsMachine, written: &[u64], event: RowsEvent) -> (Code, RowsAction) {
    let mut status = Status::new();
    let mut action = RowsAction::default();
    let host = RowsHost {
        written: written.as_ptr(),
        nwritten: written.len(),
    };
    // SAFETY: a local machine, regions, event, action and status.
    let code = unsafe {
        tess_rows_step(
            machine.as_mut_ptr().cast(),
            &raw const host,
            &raw const event,
            &raw mut action,
            &raw mut status,
        )
    };
    (code, action)
}

fn grown(memory: u64, records: u64) -> RowsEvent {
    RowsEvent {
        kind: ROWS_GROWN,
        memory,
        records,
        ..RowsEvent::default()
    }
}

fn rows_event(kind: u32) -> RowsEvent {
    RowsEvent {
        kind,
        ..RowsEvent::default()
    }
}

#[test]
fn a_grouping_s_rows_freeze_and_are_read_by_the_entry_points() {
    let params = rows_params(1 << 20);
    let mut machine = RowsMachine::default();
    assert_eq!(rows_init(&mut machine, &params), Code::Ok);
    let fill = (1 << 20) / 8 * 7;
    let quiet = RowsAction::default();
    // At seven eighths, or with fewer groups than a batch, it waits.
    assert_eq!(
        rows_step(&mut machine, &[], grown(fill, 64)),
        (Code::Ok, quiet)
    );
    assert_eq!(
        rows_step(&mut machine, &[], grown(fill + 1, 63)),
        (Code::Ok, quiet)
    );
    let (code, freeze) = rows_step(&mut machine, &[], grown(fill + 1, 64));
    assert_eq!(code, Code::Ok);
    assert_eq!((freeze.kind, freeze.level), (ROWS_FREEZE, 0));
    assert_eq!(freeze.plan.partitions, 4);
    assert_eq!(freeze.plan.chunk_len, 1024);
    assert_eq!(
        rows_step(&mut machine, &[], rows_event(ROWS_WRITTEN)),
        (Code::Ok, quiet)
    );
    // The partitions with rows, in turn; then the level is done.
    let written = [0, 3, 0, 5];
    let below = RowsTable {
        partition: 1,
        level: 1,
        shift: freeze.plan.below,
        last: u32::from(!freeze.plan.deeper),
    };
    let next = rows_event(ROWS_NEXT);
    let (code, read) = rows_step(&mut machine, &written, next);
    assert_eq!((code, read.kind, read.table), (Code::Ok, ROWS_READ, below));
    let (code, read) = rows_step(&mut machine, &written, next);
    assert_eq!(
        (code, read.kind, read.table),
        (
            Code::Ok,
            ROWS_READ,
            RowsTable {
                partition: 3,
                ..below
            }
        )
    );
    let (code, done) = rows_step(&mut machine, &written, next);
    assert_eq!((code, done.kind), (Code::Ok, ROWS_LEVEL_DONE));
    // The table of a partition read freezes into the level below it.
    let mut table = RowsMachine::default();
    assert_eq!(rows_read(&mut table, &params, &below), Code::Ok);
    let (code, freeze) = rows_step(&mut table, &[], grown(fill + 1, 64));
    assert_eq!(code, Code::Ok);
    assert_eq!(
        (freeze.kind == ROWS_FREEZE, freeze.level),
        (below.last == 0, if below.last == 0 { 1 } else { 0 })
    );
    // The last table never freezes.
    let mut last = RowsMachine::default();
    let table = RowsTable { last: 1, ..below };
    assert_eq!(rows_read(&mut last, &params, &table), Code::Ok);
    assert_eq!(
        rows_step(&mut last, &[], grown(u64::MAX, u64::MAX)),
        (Code::Ok, quiet)
    );
    assert_eq!(ROWS_DONE, 0);
}

#[test]
fn the_rows_entry_points_refuse_what_is_no_step() {
    let params = rows_params(1 << 20);
    let mut machine = RowsMachine::default();
    let mut status = Status::new();
    // SAFETY: null places, which the entry points refuse.
    unsafe {
        let code = tess_rows_init(std::ptr::null_mut(), &raw const params, &raw mut status);
        assert_eq!(code, Code::InvalidArgument);
        let code = tess_rows_init(
            machine.as_mut_ptr().cast(),
            std::ptr::null(),
            &raw mut status,
        );
        assert_eq!(code, Code::InvalidArgument);
    }
    let zero = RowsParams { limit: 0, ..params };
    assert_eq!(rows_init(&mut machine, &zero), Code::InvalidArgument);
    let table = RowsTable {
        partition: 0,
        level: 1,
        shift: 2,
        last: 2,
    };
    assert_eq!(
        rows_read(&mut machine, &params, &table),
        Code::InvalidArgument
    );
    assert_eq!(rows_init(&mut machine, &params), Code::Ok);
    let made = machine;
    // A wrong call leaves the machine as it was.
    let wrong = [
        rows_event(9),
        RowsEvent {
            spare: 1,
            ..grown(1 << 20, 64)
        },
        rows_event(ROWS_WRITTEN),
        rows_event(ROWS_NEXT),
    ];
    for event in wrong {
        let (code, _) = rows_step(&mut machine, &[], event);
        assert_eq!(code, Code::InvalidArgument, "{event:?}");
        assert_eq!(machine, made, "{event:?}");
    }
    let (code, freeze) = rows_step(&mut machine, &[], grown(1 << 20, 64));
    assert_eq!((code, freeze.kind), (Code::Ok, ROWS_FREEZE));
    let (code, _) = rows_step(&mut machine, &[], rows_event(ROWS_WRITTEN));
    assert_eq!(code, Code::Ok);
    let reading = machine;
    let (code, _) = rows_step(&mut machine, &[1, 1, 1], rows_event(ROWS_NEXT));
    assert_eq!(code, Code::InvalidArgument);
    assert_eq!(machine, reading);
    let mut garbage = [u64::MAX; 13];
    let (code, _) = rows_step(&mut garbage, &[], rows_event(ROWS_NEXT));
    assert_eq!(code, Code::InvalidArgument);
}

#[test]
fn the_rows_layout_is_its_types() {
    let sizes = [
        (ROWS_MACHINE_SIZE, size_of::<RowsMachine>()),
        (ROWS_PARAMS_SIZE, size_of::<RowsParams>()),
        (ROWS_PARAMS_LEAST_OFFSET, offset_of!(RowsParams, least)),
        (ROWS_HOST_SIZE, size_of::<RowsHost>()),
        (ROWS_EVENT_SIZE, size_of::<RowsEvent>()),
        (ROWS_ACTION_SIZE, size_of::<RowsAction>()),
        (ROWS_ACTION_TABLE_OFFSET, offset_of!(RowsAction, table)),
        (99, 0),
    ];
    for (what, size) in sizes {
        assert_eq!(tess_rows_layout(what), size, "{what}");
    }
}

#[test]
fn the_rows_entry_points_write_places_that_hold_nothing_yet() {
    use std::mem::MaybeUninit;

    let params = rows_params(1 << 20);
    let mut machine = MaybeUninit::<RowsMachine>::uninit();
    let mut action = MaybeUninit::<RowsAction>::uninit();
    let mut status = Status::new();
    // SAFETY: places valid for writes, which the entry points write whole.
    let code = unsafe {
        tess_rows_init(
            machine.as_mut_ptr().cast(),
            &raw const params,
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok, "{}", status.message());
    let host = RowsHost {
        written: std::ptr::null(),
        nwritten: 0,
    };
    let event = grown(1 << 20, 64);
    // SAFETY: the machine is made; the action a place valid for a write.
    let code = unsafe {
        tess_rows_step(
            machine.as_mut_ptr().cast(),
            &raw const host,
            &raw const event,
            action.as_mut_ptr(),
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok, "{}", status.message());
    // SAFETY: written whole by the step.
    assert_eq!(unsafe { action.assume_init() }.kind, ROWS_FREEZE);
    let table = RowsTable::default();
    let mut other = MaybeUninit::<RowsMachine>::uninit();
    // SAFETY: as above.
    let code = unsafe {
        tess_rows_read(
            other.as_mut_ptr().cast(),
            &raw const params,
            &raw const table,
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok, "{}", status.message());
}
