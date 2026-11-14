//! The machine of a build by several processes ([`relspill::shared`]),
//! declared in `include/tessera/spill.h`. Each participant keeps its own
//! machine and steps it with what its barrier and its build saw; the
//! words every participant shares come in the host at each step.

use std::ffi::c_int;
use std::mem::{offset_of, size_of};
use std::sync::atomic::AtomicU64;

use anyhow::{Context, Result, bail, ensure};
use relspill::phases::COUNTER_WORDS;
use relspill::shared::{Action, Build, Event, Host, Params};

use super::shared_spill::{check_aligned, check_spill_words};
use super::status::{Code, Status, guard};
use super::table::{output, stop_word};

/// `TessSharedBuildParams`: what every participant of a build shares;
/// see [`Params`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct SharedBuildParams {
    pub start: f64,
    pub target: f64,
    pub spilled: f64,
    pub per_check: u32,
    pub spare: u32,
}

impl SharedBuildParams {
    fn params(&self) -> Params {
        Params {
            start: self.start,
            target: self.target,
            spilled: self.spilled,
            per_check: self.per_check,
            spare: self.spare,
        }
    }
}

/// `TessSharedBuildHost`: the build's counters and the table's spill
/// words, shared, and this participant's flags of the partitions it
/// keeps in memory; see [`Host`].
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct SharedBuildHost {
    pub counters: *mut u64,
    pub spill: *mut u64,
    pub nspill: usize,
    pub resident: *const u8,
    pub nresident: usize,
}

/// `TessSharedBuildEvent`: a step of a participant; see [`Event`].
/// `phase` is that of [`SHARED_ATTACHED`]; `flag` whether the barrier
/// elected the participant ([`SHARED_ARRIVED`]), found it the last
/// ([`SHARED_LEFT`]), whether a batch sends ([`SHARED_BATCH`]) and
/// whether a send freed bytes ([`SHARED_SENT`]).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct SharedBuildEvent {
    pub kind: u32,
    pub flag: u32,
    pub phase: u32,
    pub spare: u32,
}

/// `TessSharedBuildEventKind`, of a [`SharedBuildEvent`].
pub const SHARED_DONE: u32 = 1;
pub const SHARED_ATTACHED: u32 = 2;
pub const SHARED_ARRIVED: u32 = 3;
pub const SHARED_LEFT: u32 = 4;
pub const SHARED_BATCH: u32 = 5;
pub const SHARED_SENT: u32 = 6;

/// `TessSharedBuildAction`: what a participant does next; see
/// [`Action`]. `partition` is that of [`SHARED_WRITE`], [`SHARED_KEEP`],
/// [`SHARED_DEMOTE`] and [`SHARED_SEND`]; `flag` whether the table split
/// ([`SHARED_PROBE`]).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SharedBuildAction {
    pub kind: u32,
    pub partition: u32,
    pub flag: u32,
    pub spare: u32,
}

/// `TessSharedBuildActionKind`, of a [`SharedBuildAction`].
pub const SHARED_NOTHING: u32 = 0;
pub const SHARED_ATTACH: u32 = 1;
pub const SHARED_ARRIVE_AND_WAIT: u32 = 2;
pub const SHARED_ARRIVE_AND_DETACH: u32 = 3;
pub const SHARED_DETACH: u32 = 4;
pub const SHARED_BUILD: u32 = 5;
pub const SHARED_FLUSH: u32 = 6;
pub const SHARED_WRITE: u32 = 7;
pub const SHARED_KEEP: u32 = 8;
pub const SHARED_FLUSH_END: u32 = 9;
pub const SHARED_SIZE: u32 = 10;
pub const SHARED_LINK: u32 = 11;
pub const SHARED_OUTER: u32 = 12;
pub const SHARED_PROBE: u32 = 13;
pub const SHARED_FREE: u32 = 14;
pub const SHARED_FINISHED: u32 = 15;
pub const SHARED_DEMOTE: u32 = 16;
pub const SHARED_SEND: u32 = 17;

impl SharedBuildEvent {
    fn event(&self) -> Result<Event> {
        ensure!(
            self.spare == 0,
            "a shared build event with a spare word of {}",
            self.spare
        );
        let flag = match self.flag {
            0 => false,
            1 => true,
            flag => bail!("a shared build event with a flag of {flag}"),
        };
        Ok(match self.kind {
            SHARED_DONE => Event::Done,
            SHARED_ATTACHED => Event::Attached { phase: self.phase },
            SHARED_ARRIVED => Event::Arrived { elected: flag },
            SHARED_LEFT => Event::Left { last: flag },
            SHARED_BATCH => Event::Batch { evict: flag },
            SHARED_SENT => Event::Sent { freed: flag },
            kind => bail!("a shared build event of kind {kind}"),
        })
    }
}

impl SharedBuildAction {
    fn of(action: Action) -> Self {
        let (kind, partition, flag) = match action {
            Action::Done => (SHARED_NOTHING, 0, false),
            Action::Attach => (SHARED_ATTACH, 0, false),
            Action::ArriveAndWait => (SHARED_ARRIVE_AND_WAIT, 0, false),
            Action::ArriveAndDetach => (SHARED_ARRIVE_AND_DETACH, 0, false),
            Action::Detach => (SHARED_DETACH, 0, false),
            Action::Build => (SHARED_BUILD, 0, false),
            Action::Flush => (SHARED_FLUSH, 0, false),
            Action::Write(partition) => (SHARED_WRITE, partition, false),
            Action::Keep(partition) => (SHARED_KEEP, partition, false),
            Action::FlushEnd => (SHARED_FLUSH_END, 0, false),
            Action::Size => (SHARED_SIZE, 0, false),
            Action::Link => (SHARED_LINK, 0, false),
            Action::Outer => (SHARED_OUTER, 0, false),
            Action::Probe { split } => (SHARED_PROBE, 0, split),
            Action::Free => (SHARED_FREE, 0, false),
            Action::Finished => (SHARED_FINISHED, 0, false),
            Action::Demote(partition) => (SHARED_DEMOTE, partition, false),
            Action::Send(partition) => (SHARED_SEND, partition, false),
        };
        Self {
            kind,
            partition,
            flag: u32::from(flag),
            spare: 0,
        }
    }
}

/// `tess_shared_build_init`: make `*machine` the machine of a
/// participant that has not attached yet.
///
/// # Safety
///
/// `machine` null or valid for a write of a machine, whatever it holds
/// before; `params` null or readable; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_shared_build_init(
    machine: *mut Build,
    params: *const SharedBuildParams,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let machine = output(machine, "a shared build's machine")?;
            let params = params.as_ref().context("null shared build parameters")?;
            machine.write(Build::new(&params.params())?);
            Ok(())
        })
    }
}

/// `tess_shared_build_step`: give `*machine` the participant's `event`
/// with the regions of `host`, and its next action into `*action`.
///
/// # Safety
///
/// `machine` null or a machine this process alone uses; `host` null or
/// readable, its `counters` null, misaligned, or valid for reads and
/// writes of `TESS_BUILD_COUNTER_WORDS` words, its `spill` likewise of
/// `nspill` words, both reached only through the build's and the spill's
/// calls here or in other processes, and its `resident` null or valid for
/// reads of `nresident` bytes; `event` null or readable; `action` null or
/// valid for a write, whatever it holds before; `status` as for every
/// entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_shared_build_step(
    machine: *mut Build,
    host: *const SharedBuildHost,
    event: *const SharedBuildEvent,
    action: *mut SharedBuildAction,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            ensure!(
                machine.is_aligned(),
                "a shared build's machine not aligned to 8"
            );
            // SAFETY: any bytes are a machine, whose steps check them.
            let machine = machine.as_mut().context("a null shared build's machine")?;
            let host = host.as_ref().context("a null shared build host")?;
            let event = event
                .as_ref()
                .context("a null shared build event")?
                .event()?;
            let out = output(action, "a shared build action")?;
            check_aligned(host.counters, "build counters")?;
            check_spill_words(host.spill, host.nspill)?;
            // SAFETY: checked as above; an `AtomicU64` has the size and
            // alignment of a `u64`.
            let counters = &*host.counters.cast::<[AtomicU64; COUNTER_WORDS]>();
            let spill = std::slice::from_raw_parts(host.spill.cast::<AtomicU64>(), host.nspill);
            let resident = if host.nresident == 0 {
                &[]
            } else {
                ensure!(
                    !host.resident.is_null(),
                    "null flags of resident partitions"
                );
                std::slice::from_raw_parts(host.resident, host.nresident)
            };
            let host = Host {
                counters,
                spill,
                resident,
            };
            out.write(SharedBuildAction::of(machine.step(event, &host)?));
            Ok(())
        })
    }
}

/// `tess_shared_build_stop`: before the participant leaves while it
/// probes, its share of the outer side not done, mark `stopped`, the word
/// the table's participants share; see [`Build::stop`].
///
/// # Safety
///
/// `machine` null or a machine this process alone uses; `stopped` as for
/// `tess_build_stop`; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_shared_build_stop(
    machine: *const Build,
    stopped: *mut u64,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            ensure!(
                machine.is_aligned(),
                "a shared build's machine not aligned to 8"
            );
            let machine = machine.as_ref().context("a null shared build's machine")?;
            let word = stop_word(stopped)?;
            machine.stop(&word);
            Ok(())
        })
    }
}

/// `TessSharedBuildLayout`: what [`tess_shared_build_layout`] gives the
/// size or offset of.
pub const SHARED_MACHINE_SIZE: c_int = 1;
pub const SHARED_PARAMS_SIZE: c_int = 2;
pub const SHARED_PARAMS_PER_CHECK_OFFSET: c_int = 3;
pub const SHARED_HOST_SIZE: c_int = 4;
pub const SHARED_HOST_NRESIDENT_OFFSET: c_int = 5;
pub const SHARED_EVENT_SIZE: c_int = 6;
pub const SHARED_ACTION_SIZE: c_int = 7;

/// `tess_shared_build_layout`: the size or offset of a
/// `TessSharedBuildLayout` for the C side's mirrors to check; 0 for
/// another code.
#[unsafe(no_mangle)]
pub extern "C" fn tess_shared_build_layout(what: c_int) -> usize {
    match what {
        SHARED_MACHINE_SIZE => size_of::<Build>(),
        SHARED_PARAMS_SIZE => size_of::<SharedBuildParams>(),
        SHARED_PARAMS_PER_CHECK_OFFSET => offset_of!(SharedBuildParams, per_check),
        SHARED_HOST_SIZE => size_of::<SharedBuildHost>(),
        SHARED_HOST_NRESIDENT_OFFSET => offset_of!(SharedBuildHost, nresident),
        SHARED_EVENT_SIZE => size_of::<SharedBuildEvent>(),
        SHARED_ACTION_SIZE => size_of::<SharedBuildAction>(),
        _ => 0,
    }
}
