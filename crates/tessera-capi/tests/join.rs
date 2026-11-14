//! The machine of a join's spill through its C entry points: a build
//! that spills step by step, the level below, the refusals and the
//! layout of the mirrors.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test reports a failure by panicking"
)]

use std::mem::{offset_of, size_of};

use tessera_capi::c::{
    Code, JOIN_ACTION_BELOW_OFFSET, JOIN_ACTION_PLAN_OFFSET, JOIN_ACTION_SIZE, JOIN_ADD_CHUNK,
    JOIN_BELOW_SHIFT_OFFSET, JOIN_BELOW_SIZE, JOIN_BUILD_END, JOIN_DEMOTE, JOIN_DONE,
    JOIN_EVENT_MEMORY_OFFSET, JOIN_EVENT_SIZE, JOIN_EVICT, JOIN_FLUSH, JOIN_FULL, JOIN_GROWN,
    JOIN_HOST_NROUTES_OFFSET, JOIN_HOST_SIZE, JOIN_INNER, JOIN_LEVEL_DONE, JOIN_MACHINE_SIZE,
    JOIN_MADE, JOIN_NEXT, JOIN_OPEN, JOIN_OUTER_END, JOIN_PARAMS_SIZE, JOIN_PARAMS_TAIL_OFFSET,
    JOIN_RELEASE, JOIN_SENT, JOIN_SPLIT, JOIN_START, JOIN_TAKE, JoinAction, JoinBelow, JoinEvent,
    JoinHost, JoinParams, SpillLevel, SpillPlan, SpillShape, Status, tess_join_below,
    tess_join_given, tess_join_init, tess_join_layout, tess_join_step, tess_spill_expected,
    tess_spill_plan, tess_table_spill_add_bytes,
};

/// The words of a machine.
type Machine = [u64; 26];

fn params(limit: u64) -> JoinParams {
    JoinParams {
        limit,
        shape: SpillShape {
            reserve: 4096,
            share: 16,
            min_chunk: 1024,
            max_chunk: 64 * 1024,
            record: 32,
            min_partitions: 4,
            max_partitions: 64,
        },
        outer_words: 2,
        outer_values: 1,
        preserve_inner: 0,
        page: 512,
        start: 1.0,
        target: 1.0,
        spilled: 0.0,
        resident: 0.25,
        tail: 1.0,
        split_room: 2.0 / 3.0,
        split_key: 0.9,
    }
}

/// A level's regions as C keeps them.
struct Level {
    words: Vec<u64>,
    rows: Vec<u64>,
    total: u64,
    outer: Vec<u64>,
    disk: Vec<u64>,
    routes: Vec<u8>,
}

impl Level {
    fn none() -> Self {
        Self {
            words: Vec::new(),
            rows: Vec::new(),
            total: 0,
            outer: Vec::new(),
            disk: Vec::new(),
            routes: Vec::new(),
        }
    }

    fn of(plan: &SpillPlan) -> Self {
        let partitions = plan.partitions as usize;
        Self {
            words: vec![0; plan.words],
            rows: vec![0; partitions],
            total: 0,
            outer: vec![0; partitions],
            disk: vec![0; partitions],
            routes: vec![u8::MAX; partitions],
        }
    }

    fn host(&mut self) -> JoinHost {
        JoinHost {
            words: self.words.as_mut_ptr(),
            nwords: self.words.len(),
            rows: self.rows.as_ptr(),
            nrows: self.rows.len(),
            total: self.total,
            outer: self.outer.as_ptr(),
            nouter: self.outer.len(),
            disk: self.disk.as_ptr(),
            ndisk: self.disk.len(),
            routes: self.routes.as_mut_ptr(),
            nroutes: self.routes.len(),
        }
    }
}

fn init(machine: &mut Machine, params: &JoinParams) -> Code {
    let mut status = Status::new();
    // SAFETY: a local machine, parameters and status.
    unsafe { tess_join_init(machine.as_mut_ptr().cast(), params, &raw mut status) }
}

/// A step: its code and its action.
fn step(machine: &mut Machine, level: &mut Level, event: JoinEvent) -> (Code, JoinAction) {
    let mut status = Status::new();
    let mut action = JoinAction::default();
    let host = level.host();
    // SAFETY: a local machine, regions, event, action and status.
    let code = unsafe {
        tess_join_step(
            machine.as_mut_ptr().cast(),
            &raw const host,
            &raw const event,
            &raw mut action,
            &raw mut status,
        )
    };
    (code, action)
}

fn event(kind: u32) -> JoinEvent {
    JoinEvent {
        kind,
        ..JoinEvent::default()
    }
}

fn full(side: u32, partition: u32, tail: bool) -> JoinEvent {
    JoinEvent {
        side,
        partition,
        flag: u32::from(tail),
        ..event(JOIN_FULL)
    }
}

fn sent(memory: u64) -> JoinEvent {
    JoinEvent {
        flag: 1,
        memory,
        ..event(JOIN_SENT)
    }
}

fn kind(kind: u32) -> JoinAction {
    JoinAction {
        kind,
        ..JoinAction::default()
    }
}

/// The plan `tess_spill_plan` gives the first level of `params`.
fn first_plan(params: &JoinParams, expected: f64) -> SpillPlan {
    let mut status = Status::new();
    let mut plan = SpillPlan::default();
    let level = SpillLevel {
        expected,
        limit: params.limit as usize,
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
    assert_eq!(code, Code::Ok, "{}", status.message());
    plan
}

#[test]
fn a_join_steps_through_its_build_by_the_entry_points() {
    let params = params(1 << 20);
    let mut machine: Machine = [0; 26];
    assert_eq!(init(&mut machine, &params), Code::Ok);
    let mut none = Level::none();
    // A batch that fits; then one past the limit.
    let inner = |needed| JoinEvent {
        held: 600_000,
        rows: 3000,
        estimate: 30_000.0,
        memory: needed,
        ..event(JOIN_INNER)
    };
    assert_eq!(
        step(&mut machine, &mut none, inner(1 << 20)),
        (Code::Ok, kind(JOIN_DONE))
    );
    let (code, start) = step(&mut machine, &mut none, inner((1 << 20) + 1));
    assert_eq!((code, start.kind), (Code::Ok, JOIN_START));
    let expected = tess_spill_expected(600_000.0, 3000.0, 30_000.0, 1);
    assert_eq!(start.plan, first_plan(&params, expected));
    // Two words, a lane of NULL bits, four rows, or the plan's chunk.
    assert_eq!(start.outer_len, start.plan.chunk_len.max(16 + 8 * 4 * 3));
    let mut level = Level::of(&start.plan);
    assert_eq!(
        step(&mut machine, &mut level, event(JOIN_MADE)),
        (Code::Ok, kind(JOIN_DONE))
    );
    assert_eq!(
        step(&mut machine, &mut level, full(0, 1, true)),
        (Code::Ok, kind(JOIN_ADD_CHUNK))
    );
    // Partition 1 holds the most: past the limit it goes to disk.
    let mut status = Status::new();
    let mut over = false;
    // SAFETY: the level's words, a local flag and status.
    let code = unsafe {
        tess_table_spill_add_bytes(
            level.words.as_mut_ptr(),
            level.words.len(),
            false,
            1 << 20,
            1,
            &raw mut over,
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok, "{}", status.message());
    level.rows[1] = 2000;
    level.rows[0] = 3000;
    level.total = 5000;
    let grown = JoinEvent {
        memory: 2 << 20,
        ..event(JOIN_GROWN)
    };
    let demote = JoinAction {
        partition: 1,
        ..kind(JOIN_DEMOTE)
    };
    assert_eq!(step(&mut machine, &mut level, grown), (Code::Ok, demote));
    assert_eq!(
        step(&mut machine, &mut level, sent(0)),
        (Code::Ok, kind(JOIN_DONE))
    );
    // On disk, its tail goes when full.
    assert_eq!(
        step(&mut machine, &mut level, full(0, 1, true)),
        (Code::Ok, kind(JOIN_FLUSH))
    );
    // Sent again with spilled weight 0: none; the build ends with routes.
    assert_eq!(
        step(&mut machine, &mut level, event(JOIN_BUILD_END)),
        (Code::Ok, kind(JOIN_DONE))
    );
    // Partition 1 on disk; the others in memory hold enough of the rows.
    assert!(
        level
            .routes
            .iter()
            .enumerate()
            .all(|(partition, &route)| route == u8::from(partition == 1))
    );
    assert_eq!(
        step(&mut machine, &mut level, full(1, 1, false)),
        (Code::Ok, kind(JOIN_ADD_CHUNK))
    );
    assert_eq!(
        step(&mut machine, &mut level, event(JOIN_OUTER_END)),
        (Code::Ok, kind(JOIN_DONE))
    );
    // Partition 1 on disk, with outer rows: joined whole; then the end.
    level.outer[1] = 10;
    level.disk[1] = 1000;
    let next = JoinEvent {
        memory: 0,
        ..event(JOIN_NEXT)
    };
    let open = JoinAction {
        partition: 1,
        ..kind(JOIN_OPEN)
    };
    assert_eq!(step(&mut machine, &mut level, next), (Code::Ok, open));
    assert_eq!(
        step(&mut machine, &mut level, next),
        (Code::Ok, kind(JOIN_LEVEL_DONE))
    );
}

#[test]
fn the_tail_of_a_partition_on_disk_is_evicted_when_it_weighs() {
    let params = JoinParams {
        spilled: 1.0,
        ..params(1 << 20)
    };
    let mut machine: Machine = [0; 26];
    assert_eq!(init(&mut machine, &params), Code::Ok);
    let past = JoinEvent {
        memory: (1 << 20) + 1,
        ..event(JOIN_INNER)
    };
    let (_, start) = step(&mut machine, &mut Level::none(), past);
    let mut level = Level::of(&start.plan);
    step(&mut machine, &mut level, event(JOIN_MADE));
    let add = |level: &mut Level, partition: i32, bytes: i64| {
        let mut status = Status::new();
        let mut over = false;
        // SAFETY: the level's words, a local flag and status.
        let code = unsafe {
            tess_table_spill_add_bytes(
                level.words.as_mut_ptr(),
                level.words.len(),
                false,
                bytes,
                partition,
                &raw mut over,
                &raw mut status,
            )
        };
        assert_eq!(code, Code::Ok, "{}", status.message());
    };
    add(&mut level, 2, 1 << 20);
    level.rows = vec![10; level.rows.len()];
    level.total = 10 * level.rows.len() as u64;
    let grown = JoinEvent {
        memory: 2 << 20,
        ..event(JOIN_GROWN)
    };
    let (code, action) = step(&mut machine, &mut level, grown);
    assert_eq!(
        (code, action.kind, action.partition),
        (Code::Ok, JOIN_DEMOTE, 2)
    );
    // Its tail still past the limit: chosen again, on disk, it evicts.
    let (code, action) = step(&mut machine, &mut level, sent(2 << 20));
    assert_eq!(
        (code, action.kind, action.partition),
        (Code::Ok, JOIN_EVICT, 2)
    );
    add(&mut level, 2, -(1 << 20));
    assert_eq!(
        step(&mut machine, &mut level, sent(0)),
        (Code::Ok, kind(JOIN_DONE))
    );
    // No outer row came for it: it is released, and the level is done.
    for event in [event(JOIN_BUILD_END), event(JOIN_OUTER_END)] {
        assert_eq!(
            step(&mut machine, &mut level, event),
            (Code::Ok, kind(JOIN_DONE))
        );
    }
    let release = JoinAction {
        partition: 2,
        ..kind(JOIN_RELEASE)
    };
    assert_eq!(
        step(&mut machine, &mut level, event(JOIN_NEXT)),
        (Code::Ok, release)
    );
    assert_eq!(
        step(&mut machine, &mut level, event(JOIN_NEXT)),
        (Code::Ok, kind(JOIN_LEVEL_DONE))
    );
}

#[test]
fn a_level_weighs_the_rows_its_host_counts() {
    let params = params(1 << 20);
    let mut machine: Machine = [0; 26];
    assert_eq!(init(&mut machine, &params), Code::Ok);
    let past = JoinEvent {
        memory: (1 << 20) + 1,
        ..event(JOIN_INNER)
    };
    let (_, start) = step(&mut machine, &mut Level::none(), past);
    let mut level = Level::of(&start.plan);
    step(&mut machine, &mut level, event(JOIN_MADE));
    let mut status = Status::new();
    let mut over = false;
    // SAFETY: the level's words, a local flag and status.
    let code = unsafe {
        tess_table_spill_add_bytes(
            level.words.as_mut_ptr(),
            level.words.len(),
            false,
            2 << 20,
            1,
            &raw mut over,
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok, "{}", status.message());
    // 100 rows in memory of the 10 000 of the level: they go too.
    level.rows[0] = 100;
    level.total = 10_000;
    let grown = JoinEvent {
        memory: 2 << 20,
        ..event(JOIN_GROWN)
    };
    let (_, first) = step(&mut machine, &mut level, grown);
    assert_eq!((first.kind, first.partition), (JOIN_DEMOTE, 1));
    let (_, next) = step(&mut machine, &mut level, sent(0));
    assert_eq!((next.kind, next.partition), (JOIN_DEMOTE, 0));
}

#[test]
fn a_level_below_is_planned_through_its_entry_point() {
    let params = params(1 << 20);
    let below = |level: u32, shift: u32| {
        let mut machine: Machine = [0; 26];
        let mut status = Status::new();
        let mut action = JoinAction::default();
        let below = JoinBelow {
            partition: 1,
            level,
            expected: 4 << 20,
            limit: 1 << 19,
            shift,
            spare: 0,
        };
        // SAFETY: local arguments.
        let code = unsafe {
            tess_join_below(
                machine.as_mut_ptr().cast(),
                &raw const params,
                &raw const below,
                &raw mut action,
                &raw mut status,
            )
        };
        (code, action, machine)
    };
    let (code, action, mut machine) = below(1, 3);
    assert_eq!((code, action.kind), (Code::Ok, JOIN_START));
    assert_eq!(action.plan.shift, 3);
    let mut level = Level::of(&action.plan);
    assert_eq!(
        step(&mut machine, &mut level, event(JOIN_MADE)),
        (Code::Ok, kind(JOIN_DONE))
    );
    assert_eq!(below(0, 3).0, Code::InvalidArgument, "a level 0");
    assert_eq!(
        below(1, 40).0,
        Code::InvalidArgument,
        "a shift past the hash"
    );
}

#[test]
fn a_shared_level_is_planned_through_its_entry_point() {
    let params = params(1 << 20);
    let given = |partitions: u32| {
        let mut machine: Machine = [0; 26];
        let mut status = Status::new();
        let mut action = JoinAction::default();
        // SAFETY: local arguments.
        let code = unsafe {
            tess_join_given(
                machine.as_mut_ptr().cast(),
                &raw const params,
                partitions,
                &raw mut action,
                &raw mut status,
            )
        };
        (code, action, machine)
    };
    let (code, action, mut machine) = given(16);
    assert_eq!((code, action.kind), (Code::Ok, JOIN_START));
    let level = SpillLevel {
        limit: 1 << 20,
        partitions: 16,
        ..SpillLevel::default()
    };
    let mut status = Status::new();
    let mut plan = SpillPlan::default();
    // SAFETY: local arguments.
    let code = unsafe {
        tess_spill_plan(
            &raw const params.shape,
            &raw const level,
            &raw mut plan,
            &raw mut status,
        )
    };
    assert_eq!((code, action.plan), (Code::Ok, plan));
    assert_eq!(
        given(3).0,
        Code::InvalidArgument,
        "partitions no shape plans"
    );
    // SAFETY: a null action, which the entry point refuses.
    let code = unsafe {
        tess_join_given(
            machine.as_mut_ptr().cast(),
            &raw const params,
            16,
            std::ptr::null_mut(),
            &raw mut status,
        )
    };
    assert_eq!(code, Code::InvalidArgument);
    // A partition one participant takes alone: whole, or split.
    let mut level = Level::of(&action.plan);
    level.rows[2] = 10;
    level.rows[3] = 10;
    level.total = 100;
    level.disk[2] = 100;
    level.disk[3] = 900_000;
    let take = |partition| JoinEvent {
        partition,
        ..event(JOIN_TAKE)
    };
    let open = JoinAction {
        partition: 2,
        ..kind(JOIN_OPEN)
    };
    assert_eq!(step(&mut machine, &mut level, take(2)), (Code::Ok, open));
    let (code, split) = step(&mut machine, &mut level, take(3));
    assert_eq!(
        (code, split.kind, split.partition),
        (Code::Ok, JOIN_SPLIT, 3)
    );
    let expected = JoinBelow {
        partition: 3,
        level: 1,
        expected: 900_000,
        limit: 1 << 20,
        shift: action.plan.below,
        spare: 0,
    };
    assert_eq!(split.below, expected);
}

#[test]
fn the_join_entry_points_refuse_what_is_no_step() {
    let mut machine: Machine = [0; 26];
    let mut none = Level::none();
    // Zeroed words are no machine.
    assert_eq!(
        step(&mut machine, &mut none, event(JOIN_BUILD_END)).0,
        Code::InvalidArgument
    );
    let mut bad = params(1 << 20);
    bad.limit = 0;
    assert_eq!(init(&mut machine, &bad), Code::InvalidArgument);
    assert_eq!(init(&mut machine, &params(1 << 20)), Code::Ok);
    let before = machine;
    for event in [
        event(JOIN_BUILD_END),
        event(99),
        JoinEvent {
            flag: 2,
            ..full(0, 0, false)
        },
        JoinEvent {
            side: 2,
            ..full(0, 0, false)
        },
    ] {
        assert_eq!(
            step(&mut machine, &mut none, event).0,
            Code::InvalidArgument,
            "{event:?}"
        );
        assert_eq!(machine, before, "{event:?}");
    }
    let mut status = Status::new();
    let mut action = JoinAction::default();
    let host = none.host();
    let inner = event(JOIN_INNER);
    // SAFETY: null and misaligned pointers, which the entry point refuses.
    unsafe {
        let words = machine.as_mut_ptr();
        let calls = [
            tess_join_step(
                std::ptr::null_mut(),
                &raw const host,
                &raw const inner,
                &raw mut action,
                &raw mut status,
            ),
            tess_join_step(
                words.cast::<u8>().add(1).cast(),
                &raw const host,
                &raw const inner,
                &raw mut action,
                &raw mut status,
            ),
            tess_join_step(
                words.cast(),
                std::ptr::null(),
                &raw const inner,
                &raw mut action,
                &raw mut status,
            ),
            tess_join_step(
                words.cast(),
                &raw const host,
                std::ptr::null(),
                &raw mut action,
                &raw mut status,
            ),
            tess_join_step(
                words.cast(),
                &raw const host,
                &raw const inner,
                std::ptr::null_mut(),
                &raw mut status,
            ),
            tess_join_init(words.cast(), std::ptr::null(), &raw mut status),
        ];
        assert!(
            calls.iter().all(|&code| code == Code::InvalidArgument),
            "{calls:?}"
        );
    }
    // Words of no spill.
    let past = JoinEvent {
        memory: (1 << 20) + 1,
        ..event(JOIN_INNER)
    };
    let (_, start) = step(&mut machine, &mut none, past);
    let mut short = Level {
        words: vec![0; 3],
        ..Level::of(&start.plan)
    };
    assert_eq!(
        step(&mut machine, &mut short, event(JOIN_MADE)).0,
        Code::InvalidArgument
    );
}

#[test]
fn the_join_layout_is_its_types() {
    let sizes = [
        (JOIN_MACHINE_SIZE, size_of::<Machine>()),
        (JOIN_PARAMS_SIZE, size_of::<JoinParams>()),
        (JOIN_PARAMS_TAIL_OFFSET, offset_of!(JoinParams, tail)),
        (JOIN_HOST_SIZE, size_of::<JoinHost>()),
        (JOIN_HOST_NROUTES_OFFSET, offset_of!(JoinHost, nroutes)),
        (JOIN_EVENT_SIZE, size_of::<JoinEvent>()),
        (JOIN_EVENT_MEMORY_OFFSET, offset_of!(JoinEvent, memory)),
        (JOIN_ACTION_SIZE, size_of::<JoinAction>()),
        (JOIN_ACTION_PLAN_OFFSET, offset_of!(JoinAction, plan)),
        (JOIN_ACTION_BELOW_OFFSET, offset_of!(JoinAction, below)),
        (JOIN_BELOW_SIZE, size_of::<JoinBelow>()),
        (JOIN_BELOW_SHIFT_OFFSET, offset_of!(JoinBelow, shift)),
        (99, 0),
    ];
    for (what, size) in sizes {
        assert_eq!(tess_join_layout(what), size, "{what}");
    }
    assert_eq!(size_of::<JoinParams>(), 8 * 17);
}
