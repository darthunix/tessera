//! The machine of a sort's runs through its C entry points: a sort that
//! spills step by step, the refusals and the layout of the mirrors.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test reports a failure by panicking"
)]

use std::mem::{MaybeUninit, offset_of, size_of};

use tessera_capi::c::{
    Code, RUNS_ACTION_PLACE_OFFSET, RUNS_ACTION_SIZE, RUNS_BLOCKS_SIZE, RUNS_DONE,
    RUNS_EVENT_LIMIT_OFFSET, RUNS_EVENT_SIZE, RUNS_GROWN, RUNS_INPUT_END, RUNS_KEEP,
    RUNS_MACHINE_SIZE, RUNS_MERGE, RUNS_NEXT, RUNS_PARAMS_BACKWARD_OFFSET, RUNS_PARAMS_SIZE,
    RUNS_PASS, RUNS_PASS_END, RUNS_READ, RUNS_RESCAN, RUNS_RUN, RunsAction, RunsBlocks, RunsEvent,
    RunsParams, RunsPlace, Status, tess_runs_init, tess_runs_layout, tess_runs_step,
};

/// The words of a machine.
type Machine = [u64; 10];

fn params(limit: u64) -> RunsParams {
    RunsParams {
        limit,
        row: 16,
        item: 12,
        chunk: 1024,
        set_runs: 4,
        max_merge: 256,
        batch_rows: 64,
        backward: 0,
    }
}

fn init(machine: &mut Machine, params: &RunsParams) -> (Code, RunsBlocks) {
    let mut status = Status::new();
    let mut blocks = RunsBlocks::default();
    // SAFETY: a local machine, parameters, blocks and status.
    let code = unsafe {
        tess_runs_init(
            machine.as_mut_ptr().cast(),
            params,
            &raw mut blocks,
            &raw mut status,
        )
    };
    (code, blocks)
}

fn step(machine: &mut Machine, event: RunsEvent) -> (Code, RunsAction) {
    let mut status = Status::new();
    let mut action = RunsAction::default();
    // SAFETY: a local machine, event, action and status.
    let code = unsafe {
        tess_runs_step(
            machine.as_mut_ptr().cast(),
            &raw const event,
            &raw mut action,
            &raw mut status,
        )
    };
    (code, action)
}

fn event(kind: u32) -> RunsEvent {
    RunsEvent {
        kind,
        ..RunsEvent::default()
    }
}

fn kind(kind: u32) -> RunsAction {
    RunsAction {
        kind,
        ..RunsAction::default()
    }
}

#[test]
fn a_sort_spills_its_runs_step_by_step_by_the_entry_points() {
    let mut machine = Machine::default();
    let (code, blocks) = init(&mut machine, &params(1024));
    assert_eq!(code, Code::Ok);
    let six = RunsBlocks {
        rows: 64,
        fan_in: 6,
        values: 4096,
    };
    assert_eq!(blocks, six);
    // Seven runs of the input, the last at its end.
    let grown = RunsEvent {
        memory: 1024,
        rows: 1,
        limit: 1024,
        ..event(RUNS_GROWN)
    };
    let run = |create, partition| RunsAction {
        place: RunsPlace {
            create: u32::from(create),
            partition,
        },
        ..kind(RUNS_RUN)
    };
    for at in 0..6_u32 {
        let place = run(at % 4 == 0, at % 4);
        assert_eq!(step(&mut machine, grown), (Code::Ok, place));
    }
    let end = RunsEvent {
        rows: 3,
        ..event(RUNS_INPUT_END)
    };
    assert_eq!(step(&mut machine, end), (Code::Ok, run(false, 2)));
    let next = event(RUNS_NEXT);
    assert_eq!(step(&mut machine, next), (Code::Ok, kind(RUNS_PASS)));
    let merge = RunsAction {
        first: 0,
        count: 6,
        ..run(true, 0)
    };
    let merge = RunsAction {
        kind: RUNS_MERGE,
        ..merge
    };
    assert_eq!(step(&mut machine, next), (Code::Ok, merge));
    let keep = RunsAction {
        first: 6,
        ..kind(RUNS_KEEP)
    };
    assert_eq!(step(&mut machine, next), (Code::Ok, keep));
    assert_eq!(step(&mut machine, next), (Code::Ok, kind(RUNS_PASS_END)));
    let read = RunsAction {
        runs: 2,
        ..kind(RUNS_READ)
    };
    assert_eq!(step(&mut machine, next), (Code::Ok, read));
    assert_eq!(step(&mut machine, event(RUNS_RESCAN)), (Code::Ok, read));
    // A sort that wrote no run sorts in memory.
    let (code, _) = init(&mut machine, &params(1 << 30));
    assert_eq!(code, Code::Ok);
    let roomy = RunsEvent {
        limit: 1 << 30,
        ..grown
    };
    assert_eq!(step(&mut machine, roomy), (Code::Ok, kind(RUNS_DONE)));
    assert_eq!(step(&mut machine, end), (Code::Ok, kind(RUNS_DONE)));
}

#[test]
fn a_batch_reports_the_bytes_and_the_count_of_its_rows() {
    let mut machine = Machine::default();
    assert_eq!(init(&mut machine, &params(64 * 1024)).0, Code::Ok);
    // Few rows of many bytes fit; many rows of few bytes, with what
    // sorting each takes, do not.
    let grown = |memory, rows| RunsEvent {
        memory,
        rows,
        limit: 64 * 1024,
        ..event(RUNS_GROWN)
    };
    assert_eq!(
        step(&mut machine, grown(60_000, 1)),
        (Code::Ok, kind(RUNS_DONE))
    );
    let (code, action) = step(&mut machine, grown(1, 60_000));
    assert_eq!((code, action.kind), (Code::Ok, RUNS_RUN));
    // The chunk the next batch may need counts too, and the limit is the
    // batch's.
    let at = 64 * 1024 - 1024 - 12;
    assert_eq!(
        step(&mut machine, grown(at, 1)),
        (Code::Ok, kind(RUNS_DONE))
    );
    let (code, action) = step(&mut machine, grown(at + 1, 1));
    assert_eq!((code, action.kind), (Code::Ok, RUNS_RUN));
    let wider = RunsEvent {
        limit: 64 * 1024 + 1,
        ..grown(at + 1, 1)
    };
    assert_eq!(step(&mut machine, wider), (Code::Ok, kind(RUNS_DONE)));
}

#[test]
fn a_sort_read_backward_merges_its_runs_into_one_by_the_entry_points() {
    let mut machine = Machine::default();
    let params = RunsParams {
        backward: 1,
        ..params(1024)
    };
    assert_eq!(init(&mut machine, &params).0, Code::Ok);
    let grown = RunsEvent {
        memory: 1024,
        rows: 1,
        limit: 1024,
        ..event(RUNS_GROWN)
    };
    assert_eq!(step(&mut machine, grown).1.kind, RUNS_RUN);
    let end = RunsEvent {
        rows: 1,
        ..event(RUNS_INPUT_END)
    };
    assert_eq!(step(&mut machine, end).1.kind, RUNS_RUN);
    let next = event(RUNS_NEXT);
    let kinds: Vec<u32> = (0..4).map(|_| step(&mut machine, next).1.kind).collect();
    assert_eq!(kinds, [RUNS_PASS, RUNS_MERGE, RUNS_PASS_END, RUNS_READ]);
}

#[test]
fn the_runs_entry_points_refuse_what_is_no_step() {
    let good = params(1024);
    let mut machine = Machine::default();
    let mut status = Status::new();
    let mut blocks = RunsBlocks::default();
    // SAFETY: null places, which the entry points refuse.
    unsafe {
        let code = tess_runs_init(
            std::ptr::null_mut(),
            &raw const good,
            &raw mut blocks,
            &raw mut status,
        );
        assert_eq!(code, Code::InvalidArgument);
        let code = tess_runs_init(
            machine.as_mut_ptr().cast(),
            &raw const good,
            std::ptr::null_mut(),
            &raw mut status,
        );
        assert_eq!(code, Code::InvalidArgument);
        let code = tess_runs_init(
            machine.as_mut_ptr().cast(),
            std::ptr::null(),
            &raw mut blocks,
            &raw mut status,
        );
        assert_eq!(code, Code::InvalidArgument);
    }
    let bad = RunsParams {
        max_merge: 1,
        ..good
    };
    assert_eq!(init(&mut machine, &bad).0, Code::InvalidArgument);
    assert_eq!(init(&mut machine, &good).0, Code::Ok);
    let made = machine;
    // A wrong call leaves the machine as it was.
    let wrong = [
        event(9),
        RunsEvent {
            spare: 1,
            ..event(RUNS_GROWN)
        },
        event(RUNS_NEXT),
        event(RUNS_RESCAN),
    ];
    for event in wrong {
        let (code, _) = step(&mut machine, event);
        assert_eq!(code, Code::InvalidArgument, "{event:?}");
        assert_eq!(machine, made, "{event:?}");
    }
    let mut garbage = [u64::MAX; 10];
    let (code, _) = step(&mut garbage, event(RUNS_NEXT));
    assert_eq!(code, Code::InvalidArgument);
    let mut status = Status::new();
    let mut action = RunsAction::default();
    let next = event(RUNS_NEXT);
    // SAFETY: null places, which the entry point refuses.
    unsafe {
        let code = tess_runs_step(
            std::ptr::null_mut(),
            &raw const next,
            &raw mut action,
            &raw mut status,
        );
        assert_eq!(code, Code::InvalidArgument);
        let code = tess_runs_step(
            machine.as_mut_ptr().cast(),
            &raw const next,
            std::ptr::null_mut(),
            &raw mut status,
        );
        assert_eq!(code, Code::InvalidArgument);
    }
}

#[test]
fn the_runs_layout_is_its_types() {
    let sizes = [
        (RUNS_MACHINE_SIZE, size_of::<Machine>()),
        (RUNS_PARAMS_SIZE, size_of::<RunsParams>()),
        (
            RUNS_PARAMS_BACKWARD_OFFSET,
            offset_of!(RunsParams, backward),
        ),
        (RUNS_BLOCKS_SIZE, size_of::<RunsBlocks>()),
        (RUNS_EVENT_SIZE, size_of::<RunsEvent>()),
        (RUNS_ACTION_SIZE, size_of::<RunsAction>()),
        (RUNS_ACTION_PLACE_OFFSET, offset_of!(RunsAction, place)),
        (RUNS_EVENT_LIMIT_OFFSET, offset_of!(RunsEvent, limit)),
        (99, 0),
    ];
    for (what, size) in sizes {
        assert_eq!(tess_runs_layout(what), size, "{what}");
    }
}

#[test]
fn the_runs_entry_points_write_places_that_hold_nothing_yet() {
    let params = params(1024);
    let mut machine = MaybeUninit::<Machine>::uninit();
    let mut blocks = MaybeUninit::<RunsBlocks>::uninit();
    let mut action = MaybeUninit::<RunsAction>::uninit();
    let mut status = Status::new();
    // SAFETY: places valid for writes, which the entry points write whole.
    let code = unsafe {
        tess_runs_init(
            machine.as_mut_ptr().cast(),
            &raw const params,
            blocks.as_mut_ptr(),
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok, "{}", status.message());
    // SAFETY: written whole by the call.
    assert_eq!(unsafe { blocks.assume_init() }.fan_in, 6);
    let grown = RunsEvent {
        memory: 1024,
        rows: 1,
        limit: 1024,
        ..event(RUNS_GROWN)
    };
    // SAFETY: the machine is made; the action a place valid for a write.
    let code = unsafe {
        tess_runs_step(
            machine.as_mut_ptr().cast(),
            &raw const grown,
            action.as_mut_ptr(),
            &raw mut status,
        )
    };
    assert_eq!(code, Code::Ok, "{}", status.message());
    // SAFETY: written whole by the step.
    assert_eq!(unsafe { action.assume_init() }.kind, RUNS_RUN);
}
