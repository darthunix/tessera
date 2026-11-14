//! The machine of a sort's runs ([`relspill::sort`]), declared in
//! `include/tessera/spill.h`. The sort keeps one machine for its runs and
//! steps it after each batch, at the end of its input, through the passes
//! of its merge and at a rescan.

use std::ffi::c_int;
use std::mem::{offset_of, size_of};

use anyhow::{Context, Result, bail, ensure};
use relspill::sort::{Action, Blocks, Event, Params, Place, Runs};

use super::status::{Code, Status, guard};
use super::table::output;

/// `TessRunsParams`: what a sort's runs share; see [`Params`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct RunsParams {
    pub limit: u64,
    pub row: u64,
    pub item: u64,
    pub chunk: u64,
    pub set_runs: u32,
    pub max_merge: u32,
    pub batch_rows: u32,
    pub backward: u32,
}

impl RunsParams {
    fn params(&self) -> Params {
        Params {
            limit: self.limit,
            row: self.row,
            item: self.item,
            chunk: self.chunk,
            set_runs: self.set_runs,
            max_merge: self.max_merge,
            batch_rows: self.batch_rows,
            backward: self.backward,
        }
    }
}

/// `TessRunsBlocks`: the blocks of a run and the runs a merge reads; see
/// [`Blocks`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RunsBlocks {
    pub rows: u32,
    pub fan_in: u32,
    pub values: u64,
}

impl RunsBlocks {
    fn of(blocks: Blocks) -> Self {
        Self {
            rows: blocks.rows,
            fan_in: blocks.fan_in,
            values: blocks.values,
        }
    }
}

/// `TessRunsEvent`: a step of a sort's runs; see [`Event`]. `memory`,
/// `rows` and `limit` are those of [`RUNS_GROWN`], `rows` that of
/// [`RUNS_INPUT_END`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct RunsEvent {
    pub kind: u32,
    pub spare: u32,
    pub memory: u64,
    pub rows: u64,
    pub limit: u64,
}

/// `TessRunsEventKind`, of a [`RunsEvent`].
pub const RUNS_GROWN: u32 = 1;
pub const RUNS_INPUT_END: u32 = 2;
pub const RUNS_NEXT: u32 = 3;
pub const RUNS_RESCAN: u32 = 4;

/// `TessRunsPlace`: where a run is written; see [`Place`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RunsPlace {
    pub create: u32,
    pub partition: u32,
}

/// `TessRunsAction`: what a sort does next with its runs; see
/// [`Action`]. `place` is that of [`RUNS_RUN`] and [`RUNS_MERGE`];
/// `first` the first run of a merge or the run kept; `count` the runs of
/// a merge; `runs` those the last merge reads.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RunsAction {
    pub kind: u32,
    pub first: u32,
    pub count: u32,
    pub runs: u32,
    pub place: RunsPlace,
}

/// `TessRunsActionKind`, of a [`RunsAction`].
pub const RUNS_DONE: u32 = 0;
pub const RUNS_RUN: u32 = 1;
pub const RUNS_PASS: u32 = 2;
pub const RUNS_MERGE: u32 = 3;
pub const RUNS_KEEP: u32 = 4;
pub const RUNS_PASS_END: u32 = 5;
pub const RUNS_READ: u32 = 6;

impl RunsAction {
    fn of(action: Action) -> Self {
        let place = |place: Place| RunsPlace {
            create: u32::from(place.create),
            partition: place.partition,
        };
        let kind = |kind| Self {
            kind,
            ..Self::default()
        };
        match action {
            Action::Done => kind(RUNS_DONE),
            Action::Run(at) => Self {
                place: place(at),
                ..kind(RUNS_RUN)
            },
            Action::Pass => kind(RUNS_PASS),
            Action::Merge {
                first,
                count,
                place: at,
            } => Self {
                first,
                count,
                place: place(at),
                ..kind(RUNS_MERGE)
            },
            Action::Keep { run } => Self {
                first: run,
                ..kind(RUNS_KEEP)
            },
            Action::PassEnd => kind(RUNS_PASS_END),
            Action::Read { runs } => Self {
                runs,
                ..kind(RUNS_READ)
            },
        }
    }
}

/// `tess_runs_init`: make `*machine` the machine of a sort's runs before
/// its input, and write the blocks of its runs into `*blocks`.
///
/// # Safety
///
/// `machine` and `blocks` null or valid for a write, whatever they hold
/// before; `params` null or readable; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_runs_init(
    machine: *mut Runs,
    params: *const RunsParams,
    blocks: *mut RunsBlocks,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let machine = output(machine, "runs' machine")?;
            let out = output(blocks, "runs' blocks")?;
            let params = params.as_ref().context("null runs parameters")?;
            let (runs, made) = Runs::new(&params.params())?;
            machine.write(runs);
            out.write(RunsBlocks::of(made));
            Ok(())
        })
    }
}

/// `tess_runs_step`: give `*machine` the sort's `event` and its next
/// action into `*action`.
///
/// # Safety
///
/// `machine` null or a machine this process alone uses; `event` null or
/// readable; `action` null or valid for a write, whatever it holds
/// before; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_runs_step(
    machine: *mut Runs,
    event: *const RunsEvent,
    action: *mut RunsAction,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            ensure!(machine.is_aligned(), "a runs' machine not aligned to 8");
            // SAFETY: any bytes are a machine, whose steps check them.
            let machine = machine.as_mut().context("a null runs' machine")?;
            let event = event.as_ref().context("a null runs event")?;
            ensure!(
                event.spare == 0,
                "a runs event with a spare word of {}",
                event.spare
            );
            let event = runs_event(event)?;
            let out = output(action, "runs action")?;
            out.write(RunsAction::of(machine.step(event)?));
            Ok(())
        })
    }
}

fn runs_event(event: &RunsEvent) -> Result<Event> {
    Ok(match event.kind {
        RUNS_GROWN => Event::Grown {
            memory: event.memory,
            rows: event.rows,
            limit: event.limit,
        },
        RUNS_INPUT_END => Event::InputEnd { rows: event.rows },
        RUNS_NEXT => Event::Next,
        RUNS_RESCAN => Event::Rescan,
        kind => bail!("a runs event of kind {kind}"),
    })
}

/// `TessRunsLayout`: what [`tess_runs_layout`] gives the size or offset
/// of.
pub const RUNS_MACHINE_SIZE: c_int = 1;
pub const RUNS_PARAMS_SIZE: c_int = 2;
pub const RUNS_PARAMS_BACKWARD_OFFSET: c_int = 3;
pub const RUNS_BLOCKS_SIZE: c_int = 4;
pub const RUNS_EVENT_SIZE: c_int = 5;
pub const RUNS_ACTION_SIZE: c_int = 6;
pub const RUNS_ACTION_PLACE_OFFSET: c_int = 7;
pub const RUNS_EVENT_LIMIT_OFFSET: c_int = 8;

/// `tess_runs_layout`: the size or offset of a `TessRunsLayout` for the
/// C side's mirrors to check; 0 for another code.
#[unsafe(no_mangle)]
pub extern "C" fn tess_runs_layout(what: c_int) -> usize {
    match what {
        RUNS_MACHINE_SIZE => size_of::<Runs>(),
        RUNS_PARAMS_SIZE => size_of::<RunsParams>(),
        RUNS_PARAMS_BACKWARD_OFFSET => offset_of!(RunsParams, backward),
        RUNS_BLOCKS_SIZE => size_of::<RunsBlocks>(),
        RUNS_EVENT_SIZE => size_of::<RunsEvent>(),
        RUNS_ACTION_SIZE => size_of::<RunsAction>(),
        RUNS_ACTION_PLACE_OFFSET => offset_of!(RunsAction, place),
        RUNS_EVENT_LIMIT_OFFSET => offset_of!(RunsEvent, limit),
        _ => 0,
    }
}
