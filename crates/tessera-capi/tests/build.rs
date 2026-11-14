//! The machine of a build by several processes through its C entry
//! points: a participant alone through a table that split and the walk of
//! its partitions on disk, the checks of its spill, the refusals and the
//! layout of the mirrors.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test reports a failure by panicking"
)]

use std::mem::{MaybeUninit, offset_of, size_of};

use tessera_capi::c::{
    Code, SHARED_ACTION_SIZE, SHARED_ALONE, SHARED_ARRIVE_AND_DETACH, SHARED_ARRIVE_AND_WAIT,
    SHARED_ARRIVED, SHARED_ATTACH, SHARED_ATTACHED, SHARED_BATCH, SHARED_BUILD, SHARED_DEMOTE,
    SHARED_DONE, SHARED_EVENT_BYTES_OFFSET, SHARED_EVENT_SIZE, SHARED_FLUSH_END, SHARED_FREE,
    SHARED_HOST_NRESIDENT_OFFSET, SHARED_HOST_SIZE, SHARED_KEEP, SHARED_LEFT, SHARED_LINK,
    SHARED_MACHINE_SIZE, SHARED_NEXT, SHARED_NOTHING, SHARED_OUTER, SHARED_PARAMS_PER_CHECK_OFFSET,
    SHARED_PARAMS_SIZE, SHARED_PLANNED, SHARED_PROBE, SHARED_ROUND, SHARED_SEND, SHARED_SENT,
    SHARED_SIZE, SHARED_SPLIT, SHARED_SWITCH, SHARED_VISIT, SHARED_VISITED, SHARED_WALKED,
    SHARED_WEIGH, SHARED_WEIGHED, SHARED_WRITE, SharedBuildAction, SharedBuildEvent,
    SharedBuildHost, SharedBuildParams, SpillWeights, Status, tess_shared_build_init,
    tess_shared_build_layout, tess_shared_build_step, tess_shared_build_stop,
    tess_table_spill_add_bytes, tess_table_spill_evict, tess_table_spill_init,
    tess_table_spill_split, tess_table_spill_words,
};

/// The words of a machine.
type Machine = [u64; 14];

fn params() -> SharedBuildParams {
    SharedBuildParams {
        start: 1.0,
        target: 0.5,
        spilled: 1.0,
        limit: 1000,
        max_chunks: 4,
        per_check: 0,
        preserve: 0,
    }
}

/// A build's shared words: its counters and a table's spill words of up
/// to 4 partitions.
struct Shared {
    counters: [u64; 4],
    spill: Vec<u64>,
}

impl Shared {
    fn new(records: u64, budget: u64) -> Self {
        let mut status = Status::new();
        let mut nwords = 0;
        // SAFETY: a local count and status.
        let code = unsafe { tess_table_spill_words(4, &raw mut nwords, &raw mut status) };
        assert_eq!(code, Code::Ok, "{}", status.message());
        let mut shared = Self {
            counters: [records, 0, 0, 0],
            spill: vec![0; nwords],
        };
        // SAFETY: the words just made, which this test alone reaches.
        let code = unsafe {
            tess_table_spill_init(
                shared.spill.as_mut_ptr(),
                shared.spill.len(),
                true,
                budget,
                &raw mut status,
            )
        };
        assert_eq!(code, Code::Ok, "{}", status.message());
        shared
    }

    fn split(&mut self, partitions: u32) {
        let mut status = Status::new();
        let mut in_force = 0;
        // SAFETY: as in `new`.
        let code = unsafe {
            tess_table_spill_split(
                self.spill.as_mut_ptr(),
                self.spill.len(),
                true,
                partitions,
                &raw mut in_force,
                &raw mut status,
            )
        };
        assert_eq!((code, in_force), (Code::Ok, partitions));
    }

    fn add(&mut self, partition: i32, bytes: i64) {
        let mut status = Status::new();
        let mut over = false;
        // SAFETY: as in `new`.
        let code = unsafe {
            tess_table_spill_add_bytes(
                self.spill.as_mut_ptr(),
                self.spill.len(),
                true,
                bytes,
                partition,
                &raw mut over,
                &raw mut status,
            )
        };
        assert_eq!(code, Code::Ok, "{}", status.message());
    }

    /// Another participant sends `partition` to disk.
    fn send(&mut self, partition: i32) {
        self.add(partition, 1);
        let weights = SpillWeights {
            start: 0.0,
            target: 0.0,
            spilled: 1.0,
            reserve: 0,
            resident: 0.0,
            per_check: 0,
        };
        let mut status = Status::new();
        let mut chosen = -1;
        // SAFETY: as in `new`.
        let code = unsafe {
            tess_table_spill_evict(
                self.spill.as_mut_ptr(),
                self.spill.len(),
                true,
                &raw const weights,
                0,
                0,
                std::ptr::null(),
                0,
                0,
                0,
                &raw mut chosen,
                &raw mut status,
            )
        };
        assert_eq!((code, chosen), (Code::Ok, partition));
        self.add(partition, -1);
    }

    fn host(&mut self, resident: &[u8]) -> SharedBuildHost {
        SharedBuildHost {
            counters: self.counters.as_mut_ptr(),
            spill: self.spill.as_mut_ptr(),
            nspill: self.spill.len(),
            resident: resident.as_ptr(),
            nresident: resident.len(),
        }
    }
}

fn init(machine: &mut Machine, params: &SharedBuildParams) -> Code {
    let mut status = Status::new();
    // SAFETY: a local machine, parameters and status.
    unsafe { tess_shared_build_init(machine.as_mut_ptr().cast(), params, &raw mut status) }
}

fn step(
    machine: &mut Machine,
    host: &SharedBuildHost,
    event: SharedBuildEvent,
) -> (Code, SharedBuildAction) {
    let mut status = Status::new();
    let mut action = SharedBuildAction::default();
    // SAFETY: a local machine, host regions, event, action and status.
    let code = unsafe {
        tess_shared_build_step(
            machine.as_mut_ptr().cast(),
            host,
            &raw const event,
            &raw mut action,
            &raw mut status,
        )
    };
    (code, action)
}

fn event(kind: u32, flag: u32) -> SharedBuildEvent {
    SharedBuildEvent {
        kind,
        flag,
        ..SharedBuildEvent::default()
    }
}

fn weighed(bytes: u64, blocks: u64, index: u64) -> SharedBuildEvent {
    SharedBuildEvent {
        bytes,
        blocks,
        index,
        ..event(SHARED_WEIGHED, 0)
    }
}

fn action(kind: u32, partition: u32) -> SharedBuildAction {
    SharedBuildAction {
        kind,
        partition,
        ..SharedBuildAction::default()
    }
}

#[test]
fn a_participant_alone_builds_a_table_that_splits_by_the_entry_points() {
    let mut shared = Shared::new(3, 1000);
    let mut machine = Machine::default();
    assert_eq!(init(&mut machine, &params()), Code::Ok);
    let none = shared.host(&[]);
    let done = event(SHARED_DONE, 0);
    assert_eq!(
        step(&mut machine, &none, done),
        (Code::Ok, action(SHARED_ATTACH, 0))
    );
    let attached = SharedBuildEvent {
        value: 0,
        ..event(SHARED_ATTACHED, 0)
    };
    assert_eq!(
        step(&mut machine, &none, attached),
        (Code::Ok, action(SHARED_BUILD, 0))
    );
    // The table splits; another participant sends partition 1, and this
    // one's batch writes it, then sends 3, the largest past the budget.
    shared.split(4);
    shared.send(1);
    shared.add(3, 1500);
    let resident = [1, 1, 1, 1];
    let host = shared.host(&resident);
    let batch = event(SHARED_BATCH, 1);
    assert_eq!(
        step(&mut machine, &host, batch),
        (Code::Ok, action(SHARED_SWITCH, 4))
    );
    assert_eq!(
        step(&mut machine, &host, done),
        (Code::Ok, action(SHARED_DEMOTE, 1))
    );
    assert_eq!(
        step(&mut machine, &host, done),
        (Code::Ok, action(SHARED_SEND, 3))
    );
    shared.add(3, -1500);
    let host = shared.host(&[1, 0, 1, 0]);
    assert_eq!(
        step(&mut machine, &host, event(SHARED_SENT, 1)),
        (Code::Ok, action(SHARED_NOTHING, 0))
    );
    let arrived = event(SHARED_ARRIVED, 1);
    let expected = [
        (done, action(SHARED_ARRIVE_AND_WAIT, 0)),
        (arrived, action(SHARED_KEEP, 0)),
        (done, action(SHARED_WRITE, 1)),
        (done, action(SHARED_KEEP, 2)),
        (done, action(SHARED_WRITE, 3)),
        (done, action(SHARED_FLUSH_END, 0)),
        (done, action(SHARED_ARRIVE_AND_WAIT, 0)),
        (arrived, action(SHARED_SIZE, 0)),
        (done, action(SHARED_WEIGH, 1)),
        // Partition 1 fits the limit with its index; 3 does not.
        (weighed(500, 1, 400), action(SHARED_WEIGH, 3)),
        (weighed(700, 2, 400), action(SHARED_ARRIVE_AND_WAIT, 0)),
        (arrived, action(SHARED_LINK, 0)),
        (done, action(SHARED_ARRIVE_AND_WAIT, 0)),
        (arrived, action(SHARED_OUTER, 0)),
        (done, action(SHARED_ARRIVE_AND_WAIT, 0)),
        (
            arrived,
            SharedBuildAction {
                flag: 1,
                ..action(SHARED_PROBE, 0)
            },
        ),
        (done, action(SHARED_ARRIVE_AND_DETACH, 0)),
        (event(SHARED_LEFT, 1), action(SHARED_FREE, 0)),
        // The walk from partition 0: 1 joined in its round, 3 alone.
        (event(SHARED_NEXT, 0), action(SHARED_VISIT, 1)),
        (event(SHARED_VISITED, 1), action(SHARED_ROUND, 1)),
        (event(SHARED_NEXT, 0), action(SHARED_VISIT, 3)),
        (event(SHARED_VISITED, 1), action(SHARED_ALONE, 3)),
        (event(SHARED_NEXT, 0), action(SHARED_WALKED, 0)),
    ];
    for (event, action) in expected {
        assert_eq!(
            step(&mut machine, &host, event),
            (Code::Ok, action),
            "{event:?}"
        );
    }
}

#[test]
fn a_table_splits_by_the_entry_points_once_its_plan_is_published() {
    let mut shared = Shared::new(0, 1000);
    let mut machine = Machine::default();
    assert_eq!(init(&mut machine, &params()), Code::Ok);
    let none = shared.host(&[]);
    let done = event(SHARED_DONE, 0);
    assert_eq!(step(&mut machine, &none, done).1, action(SHARED_ATTACH, 0));
    let attached = event(SHARED_ATTACHED, 0);
    assert_eq!(
        step(&mut machine, &none, attached).1,
        action(SHARED_BUILD, 0)
    );
    assert_eq!(
        step(&mut machine, &none, event(SHARED_BATCH, 0)),
        (Code::Ok, action(SHARED_NOTHING, 0))
    );
    assert_eq!(
        step(&mut machine, &none, event(SHARED_BATCH, 1)),
        (Code::Ok, action(SHARED_SPLIT, 0))
    );
    let planned = SharedBuildEvent {
        value: 4,
        ..event(SHARED_PLANNED, 0)
    };
    assert_eq!(
        step(&mut machine, &none, planned),
        (Code::Ok, action(SHARED_SWITCH, 4))
    );
    let host = shared.host(&[1, 1, 1, 1]);
    assert_eq!(
        step(&mut machine, &host, done),
        (Code::Ok, action(SHARED_NOTHING, 0))
    );
}

#[test]
fn the_shared_build_entry_points_refuse_what_is_no_step() {
    let mut shared = Shared::new(0, 1000);
    let mut machine = Machine::default();
    let bad = SharedBuildParams {
        preserve: 2,
        ..params()
    };
    assert_eq!(init(&mut machine, &bad), Code::InvalidArgument);
    assert_eq!(init(&mut machine, &params()), Code::Ok);
    let made = machine;
    let host = shared.host(&[]);
    let wrong = [
        event(99, 0),
        event(SHARED_DONE, 2),
        SharedBuildEvent {
            spare: 1,
            ..event(SHARED_DONE, 0)
        },
        event(SHARED_ARRIVED, 1),
    ];
    for event in wrong {
        let (code, _) = step(&mut machine, &host, event);
        assert_eq!(code, Code::InvalidArgument, "{event:?}");
        assert_eq!(machine, made, "{event:?}");
    }
    // Words of no spill, and counters of no alignment.
    let short = SharedBuildHost { nspill: 3, ..host };
    assert_eq!(
        step(&mut machine, &short, event(SHARED_DONE, 0)).0,
        Code::InvalidArgument
    );
    let crooked = SharedBuildHost {
        counters: host.counters.cast::<u8>().wrapping_add(1).cast(),
        ..host
    };
    assert_eq!(
        step(&mut machine, &crooked, event(SHARED_DONE, 0)).0,
        Code::InvalidArgument
    );
    assert_eq!(machine, made);
    let mut garbage = [u64::MAX; 14];
    assert_eq!(
        step(&mut garbage, &host, event(SHARED_DONE, 0)).0,
        Code::InvalidArgument
    );
    // A stop through a null machine is refused.
    let mut word = 0_u64;
    let mut status = Status::new();
    // SAFETY: a null machine, which the entry point refuses.
    let code = unsafe { tess_shared_build_stop(std::ptr::null(), &raw mut word, &raw mut status) };
    assert_eq!(code, Code::InvalidArgument);
}

#[test]
fn the_shared_build_layout_is_its_types() {
    let sizes = [
        (SHARED_MACHINE_SIZE, size_of::<Machine>()),
        (SHARED_PARAMS_SIZE, size_of::<SharedBuildParams>()),
        (
            SHARED_PARAMS_PER_CHECK_OFFSET,
            offset_of!(SharedBuildParams, per_check),
        ),
        (SHARED_HOST_SIZE, size_of::<SharedBuildHost>()),
        (
            SHARED_HOST_NRESIDENT_OFFSET,
            offset_of!(SharedBuildHost, nresident),
        ),
        (SHARED_EVENT_SIZE, size_of::<SharedBuildEvent>()),
        (
            SHARED_EVENT_BYTES_OFFSET,
            offset_of!(SharedBuildEvent, bytes),
        ),
        (SHARED_ACTION_SIZE, size_of::<SharedBuildAction>()),
        (99, 0),
    ];
    for (what, size) in sizes {
        assert_eq!(tess_shared_build_layout(what), size, "{what}");
    }
}

#[test]
fn the_shared_build_entry_points_write_places_that_hold_nothing_yet() {
    let mut shared = Shared::new(0, 1000);
    let params = params();
    let mut machine = MaybeUninit::<Machine>::uninit();
    let mut action = MaybeUninit::<SharedBuildAction>::uninit();
    let mut status = Status::new();
    // SAFETY: places valid for writes, which the entry points write whole.
    let code = unsafe {
        tess_shared_build_init(
            machine.as_mut_ptr().cast(),
            &raw const params,
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok, "{}", status.message());
    let host = shared.host(&[]);
    let done = event(SHARED_DONE, 0);
    // SAFETY: the machine is made; the action a place valid for a write.
    let code = unsafe {
        tess_shared_build_step(
            machine.as_mut_ptr().cast(),
            &raw const host,
            &raw const done,
            action.as_mut_ptr(),
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok, "{}", status.message());
    // SAFETY: written whole by the step.
    assert_eq!(unsafe { action.assume_init() }.kind, SHARED_ATTACH);
}
