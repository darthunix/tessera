//! The writer of a spill set's file ([`relspill::set`]), declared in
//! `include/tessera/spill.h`. The runtime's set keeps the writer's words,
//! its buffers and its block lists, and writes the ranges a step names.

use std::ffi::c_int;
use std::mem::{offset_of, size_of};

use anyhow::{Context, Result, bail, ensure};
use relspill::BlockKind;
use relspill::set::{Block, Config, Entry, Lists, MAX_RANGES, Region, Step, Writer};

use super::status::{Code, Status, guard};
use super::table::{slots, values};

/// `TessSpillList`: a partition's block list as C keeps it, `slots`
/// entries at `entries`, of which the first `count` are its blocks.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct SpillList {
    pub entries: *mut Entry,
    pub count: u64,
    pub slots: u64,
}

/// `TessSpillRange`: `len` bytes of a region from byte `start`, to the
/// file at byte `at`; `partition` names the list of a list's region.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SpillRange {
    pub region: u32,
    pub partition: u32,
    pub start: u64,
    pub len: u64,
    pub at: u64,
}

/// `TessSpillStep`: what a call of the writer asks; see [`Step`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SpillStep {
    pub kind: u32,
    pub nranges: u32,
    /// The bytes of scratch, or the entries of a list, a step needs.
    pub need: u64,
    /// Where a block written starts and the bytes it takes.
    pub offset: u64,
    pub stored: u64,
    pub ranges: [SpillRange; MAX_RANGES],
}

/// `TessSpillStepKind`: write the ranges and call again.
pub const STEP_AGAIN: u32 = 0;
/// Write the ranges: the call is done.
pub const STEP_DONE: u32 = 1;
/// Give a larger scratch region and call again.
pub const STEP_SCRATCH: u32 = 2;
/// Give the partition's list more entries and call again.
pub const STEP_LIST: u32 = 3;

/// `TessSpillRegion`: the write buffer.
pub const REGION_BUFFER: u32 = 0;
/// The scratch region.
pub const REGION_SCRATCH: u32 = 1;
/// The body the call was given.
pub const REGION_BODY: u32 = 2;
/// A partition's list, its entries as bytes.
pub const REGION_LIST: u32 = 3;

impl SpillStep {
    fn from_step(step: Step) -> Self {
        let mut out = Self::default();
        let ranges = match step {
            Step::Again(ranges) => {
                out.kind = STEP_AGAIN;
                ranges
            }
            Step::Done {
                ranges,
                offset,
                stored,
            } => {
                out.kind = STEP_DONE;
                out.offset = offset;
                out.stored = stored;
                ranges
            }
            Step::Scratch(need) => {
                out.kind = STEP_SCRATCH;
                out.need = need as u64;
                return out;
            }
            Step::List(need) => {
                out.kind = STEP_LIST;
                out.need = need as u64;
                return out;
            }
        };
        for (to, range) in out.ranges.iter_mut().zip(ranges.as_slice()) {
            let (region, partition) = match range.region {
                Region::Buffer => (REGION_BUFFER, 0),
                Region::Scratch => (REGION_SCRATCH, 0),
                Region::Body => (REGION_BODY, 0),
                Region::List(partition) => (REGION_LIST, partition),
            };
            *to = SpillRange {
                region,
                partition,
                start: range.start as u64,
                len: range.len as u64,
                at: range.at,
            };
        }
        out.nranges = ranges.as_slice().len() as u32;
        out
    }
}

/// The block lists of a set as C keeps them: `len` lists at `lists`.
struct RawLists {
    lists: *mut SpillList,
    len: usize,
}

impl Lists for RawLists {
    #[allow(
        clippy::panic,
        reason = "a list C keeps out of its contract fails the call, in the guard"
    )]
    fn list(&mut self, partition: usize) -> (&mut [Entry], &mut u64) {
        assert!(partition < self.len, "list {partition} of {}", self.len);
        // SAFETY: the entry point's contract: `len` lists, each valid,
        // with `slots` entries at `entries`, which nothing else uses
        // during the call; the list and its entries do not overlap.
        unsafe {
            let list = &mut *self.lists.add(partition);
            let entries: &mut [Entry] = if list.slots == 0 {
                &mut []
            } else {
                assert!(
                    !list.entries.is_null() && list.entries.is_aligned(),
                    "the entries of list {partition}"
                );
                std::slice::from_raw_parts_mut(list.entries, list.slots as usize)
            };
            (entries, &mut list.count)
        }
    }
}

/// The lists of `writer`'s partitions at `lists`.
fn raw_lists(writer: &Writer, lists: *mut SpillList, nlists: c_int) -> Result<RawLists> {
    ensure!(!lists.is_null() && lists.is_aligned(), "a null block list");
    ensure!(
        u32::try_from(nlists).ok() == Some(writer.partitions()),
        "{nlists} block lists for {} partitions",
        writer.partitions()
    );
    Ok(RawLists {
        lists,
        len: nlists as usize,
    })
}

/// The writer at `writer`.
///
/// # Safety
///
/// `writer` must be null or point to a writer this process alone uses.
unsafe fn writer_at<'a>(writer: *mut Writer) -> Result<&'a mut Writer> {
    ensure!(writer.is_aligned(), "a writer not aligned to 8");
    // SAFETY: the caller's contract; any bytes are a writer, whose calls
    // check them.
    unsafe { writer.as_mut() }.context("a null writer")
}

/// `tess_spill_set_init`: make `*writer` the writer of an empty set.
///
/// # Safety
///
/// `writer` must point to a writer this process alone uses; `status` as
/// for every entry point.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments, reason = "the scalars of a C entry point")]
pub unsafe extern "C" fn tess_spill_set_init(
    writer: *mut Writer,
    npartitions: c_int,
    level: u32,
    fingerprint: u64,
    max_len: u64,
    buffer_len: usize,
    shared: bool,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let writer = writer_at(writer)?;
            *writer = Writer::new(Config {
                partitions: u32::try_from(npartitions).context("negative partitions")?,
                level,
                fingerprint,
                max_len,
                buffer_len,
                shared,
            })?;
            Ok(())
        })
    }
}

/// `tess_spill_set_write`: a step of writing a block of `kind`, number
/// `number` and partition `partition`, its `len` bytes at `body`, into
/// `*step` ([`Writer::write`]).
///
/// # Safety
///
/// `writer` as for [`tess_spill_set_init`]; `buffer` must point to
/// `buffer_len` writable bytes, and `scratch` to `scratch_len` when it is
/// positive; `lists` to `nlists` lists, each with `slots` entries at
/// `entries` when `slots` is positive; `body` to `len` bytes when `len`
/// is positive; `step` must be writable; none of them overlap, and
/// nothing else uses them during the call. `status` as for every entry
/// point.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments, reason = "the regions of a C entry point")]
pub unsafe extern "C" fn tess_spill_set_write(
    writer: *mut Writer,
    buffer: *mut u8,
    buffer_len: usize,
    scratch: *mut u8,
    scratch_len: usize,
    lists: *mut SpillList,
    nlists: c_int,
    partition: c_int,
    kind: u32,
    number: u32,
    body: *const u8,
    len: usize,
    step: *mut SpillStep,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let writer = writer_at(writer)?;
            let mut lists = raw_lists(writer, lists, nlists)?;
            let kind = match kind {
                1 => BlockKind::Records,
                2 => BlockKind::Values,
                3 => BlockKind::Columns,
                other => bail!("a spilled block of unknown kind {other}"),
            };
            let block = Block {
                kind,
                number,
                partition: u32::try_from(partition).context("a negative partition")?,
                body: values(body, len, "body")?,
            };
            ensure!(step.is_aligned(), "a step not aligned to 8");
            let out = step.as_mut().context("a null step")?;
            let result = writer.write(
                slots(buffer, buffer_len, "buffer")?,
                slots(scratch, scratch_len, "scratch")?,
                &mut lists,
                block,
            )?;
            *out = SpillStep::from_step(result);
            Ok(())
        })
    }
}

/// `tess_spill_set_written`: the ranges of the last step were written
/// ([`Writer::written`]).
///
/// # Safety
///
/// As for [`tess_spill_set_init`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_spill_set_written(writer: *mut Writer, status: *mut Status) -> Code {
    // SAFETY: the caller's contract.
    unsafe { guard(status, || Ok(writer_at(writer)?.written()?)) }
}

/// `tess_spill_set_finish`: a step of finishing the set into `*step`
/// ([`Writer::finish`]).
///
/// # Safety
///
/// As for [`tess_spill_set_write`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_spill_set_finish(
    writer: *mut Writer,
    buffer: *mut u8,
    buffer_len: usize,
    lists: *mut SpillList,
    nlists: c_int,
    step: *mut SpillStep,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let writer = writer_at(writer)?;
            let mut lists = raw_lists(writer, lists, nlists)?;
            ensure!(step.is_aligned(), "a step not aligned to 8");
            let out = step.as_mut().context("a null step")?;
            let result = writer.finish(slots(buffer, buffer_len, "buffer")?, &mut lists)?;
            *out = SpillStep::from_step(result);
            Ok(())
        })
    }
}

/// `tess_spill_set_drop`: forget the blocks of `partition`
/// ([`Writer::drop_list`]).
///
/// # Safety
///
/// As for [`tess_spill_set_write`], for `writer` and `lists`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_spill_set_drop(
    writer: *mut Writer,
    lists: *mut SpillList,
    nlists: c_int,
    partition: c_int,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let writer = writer_at(writer)?;
            let mut lists = raw_lists(writer, lists, nlists)?;
            let partition = u32::try_from(partition).context("a negative partition")?;
            Ok(writer.drop_list(&mut lists, partition)?)
        })
    }
}

/// `tess_spill_set_stats`: the blocks and the bytes written, and the
/// partitions whose lists hold blocks.
///
/// # Safety
///
/// `writer` must point to a writer; each of `blocks`, `bytes` and
/// `files` must be null or writable; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_spill_set_stats(
    writer: *const Writer,
    blocks: *mut u64,
    bytes: *mut u64,
    files: *mut c_int,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            ensure!(writer.is_aligned(), "a writer not aligned to 8");
            let writer = writer.as_ref().context("a null writer")?;
            if let Some(blocks) = blocks.as_mut() {
                *blocks = writer.blocks();
            }
            if let Some(bytes) = bytes.as_mut() {
                *bytes = writer.bytes();
            }
            if let Some(files) = files.as_mut() {
                *files = c_int::try_from(writer.files()).context("files past an int")?;
            }
            Ok(())
        })
    }
}

/// `TessSpillSetLayout`: the bytes of a writer.
pub const SET_WRITER_SIZE: c_int = 0;
/// Of an entry of a list.
pub const SET_ENTRY_SIZE: c_int = 1;
/// Of a list, and where its count lies.
pub const SET_LIST_SIZE: c_int = 2;
pub const SET_LIST_COUNT_OFFSET: c_int = 3;
/// Of a range, of a step, and where a step's ranges lie.
pub const SET_RANGE_SIZE: c_int = 4;
pub const SET_STEP_SIZE: c_int = 5;
pub const SET_STEP_RANGES_OFFSET: c_int = 6;

/// `tess_spill_set_layout`: the size or offset of a `TessSpillSetLayout`
/// for the C side's mirrors to check; 0 for another code.
#[unsafe(no_mangle)]
pub extern "C" fn tess_spill_set_layout(what: c_int) -> usize {
    match what {
        SET_WRITER_SIZE => size_of::<Writer>(),
        SET_ENTRY_SIZE => size_of::<Entry>(),
        SET_LIST_SIZE => size_of::<SpillList>(),
        SET_LIST_COUNT_OFFSET => offset_of!(SpillList, count),
        SET_RANGE_SIZE => size_of::<SpillRange>(),
        SET_STEP_SIZE => size_of::<SpillStep>(),
        SET_STEP_RANGES_OFFSET => offset_of!(SpillStep, ranges),
        _ => 0,
    }
}
