//! The writer and the reader of a spill set's file ([`relspill::set`]),
//! declared in `include/tessera/spill.h`. The runtime's set keeps the
//! writer's and the readers' words, their buffers and the block lists,
//! and writes and reads the ranges a step names.

use std::ffi::c_int;
use std::mem::{offset_of, size_of};

use anyhow::{Context, Result, bail, ensure};
use relspill::set::{
    Block, Config, Entry, Lists, MAX_RANGES, ReadConfig, ReadStep, Reader, Region, Step, Writer,
};
use relspill::{BlockKind, Bounds};

use super::spill::{SpillHeader, SpillLevel, SpillPlan, SpillShape};
use super::status::{Code, Status, guard};
use super::table::{output, slots, values};

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

/// `tess_spill_set_init`: make `*writer` the writer of an empty set,
/// whose chunks of records and of columns take at most `max_chunk` body
/// bytes, and chunks of values at most `max_values`.
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
    max_chunk: u64,
    max_values: u64,
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
                bounds: Bounds {
                    chunk: max_chunk,
                    values: max_values,
                },
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
            let out = output(step, "step")?;
            let result = writer.write(
                slots(buffer, buffer_len, "buffer")?,
                slots(scratch, scratch_len, "scratch")?,
                &mut lists,
                block,
            )?;
            out.write(SpillStep::from_step(result));
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
            let out = output(step, "step")?;
            let result = writer.finish(slots(buffer, buffer_len, "buffer")?, &mut lists)?;
            out.write(SpillStep::from_step(result));
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

/// `tess_spill_set_stats`: the blocks and the bytes written, the
/// partitions whose lists hold blocks, and the longest body of values
/// written, a reader's bound for values ([`Writer::read_bounds`]).
///
/// # Safety
///
/// `writer` must point to a writer; each of `blocks`, `bytes`, `files`
/// and `values` must be null or writable; `status` as for every entry
/// point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_spill_set_stats(
    writer: *const Writer,
    blocks: *mut u64,
    bytes: *mut u64,
    files: *mut c_int,
    values: *mut u64,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            ensure!(writer.is_aligned(), "a writer not aligned to 8");
            let writer = writer.as_ref().context("a null writer")?;
            let files_written = c_int::try_from(writer.files()).context("files past an int")?;
            if !blocks.is_null() {
                output(blocks, "count of blocks")?.write(writer.blocks());
            }
            if !bytes.is_null() {
                output(bytes, "count of bytes")?.write(writer.bytes());
            }
            if !files.is_null() {
                output(files, "count of files")?.write(files_written);
            }
            if !values.is_null() {
                output(values, "longest values")?.write(writer.read_bounds().values);
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
/// Of a reader, of a step of a reader, and where its header lies.
pub const SET_READER_SIZE: c_int = 7;
pub const SET_READ_STEP_SIZE: c_int = 8;
pub const SET_READ_STEP_HEADER_OFFSET: c_int = 9;
/// Of a level's shape, its level and its plan, and where the last field
/// of each lies.
pub const SET_SHAPE_SIZE: c_int = 10;
pub const SET_SHAPE_MAX_PARTITIONS_OFFSET: c_int = 11;
pub const SET_LEVEL_SIZE: c_int = 12;
pub const SET_LEVEL_PARTITIONS_OFFSET: c_int = 13;
pub const SET_PLAN_SIZE: c_int = 14;
pub const SET_PLAN_DEEPER_OFFSET: c_int = 15;

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
        SET_READER_SIZE => size_of::<Reader>(),
        SET_READ_STEP_SIZE => size_of::<SpillReadStep>(),
        SET_READ_STEP_HEADER_OFFSET => offset_of!(SpillReadStep, header),
        SET_SHAPE_SIZE => size_of::<SpillShape>(),
        SET_SHAPE_MAX_PARTITIONS_OFFSET => offset_of!(SpillShape, max_partitions),
        SET_LEVEL_SIZE => size_of::<SpillLevel>(),
        SET_LEVEL_PARTITIONS_OFFSET => offset_of!(SpillLevel, partitions),
        SET_PLAN_SIZE => size_of::<SpillPlan>(),
        SET_PLAN_DEEPER_OFFSET => offset_of!(SpillPlan, deeper),
        _ => 0,
    }
}

/// `TessSpillReadStep`: what a call of a reader asks or found; see
/// [`ReadStep`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct SpillReadStep {
    pub kind: u32,
    pub pad: u32,
    /// Of a read: where in the file, and how many bytes.
    pub at: u64,
    pub len: u64,
    /// Of an open done: the bytes the block buffer must hold.
    pub largest: u64,
    /// Of a block read: its header.
    pub header: SpillHeader,
}

/// `TessSpillReadStepKind`: read into the page and call again.
pub const READ_PAGE: u32 = 0;
/// Read into the block buffer and call again.
pub const READ_BLOCK: u32 = 1;
/// The partition has no block.
pub const READ_EMPTY: u32 = 2;
/// The open is done.
pub const READ_READY: u32 = 3;
/// A block's header; its body waits.
pub const READ_HEADER: u32 = 4;
/// Every block was read.
pub const READ_END: u32 = 5;
/// A seek found its block.
pub const READ_FOUND: u32 = 6;

impl SpillReadStep {
    fn from_step(step: ReadStep) -> Self {
        let mut out = Self::default();
        match step {
            ReadStep::Page { at, len } => {
                (out.kind, out.at, out.len) = (READ_PAGE, at, len as u64);
            }
            ReadStep::Block { at, len } => {
                (out.kind, out.at, out.len) = (READ_BLOCK, at, len as u64);
            }
            ReadStep::Empty => out.kind = READ_EMPTY,
            ReadStep::Ready { largest } => (out.kind, out.largest) = (READ_READY, largest),
            ReadStep::Header(header) => {
                (out.kind, out.header) = (READ_HEADER, SpillHeader::from_block(header));
            }
            ReadStep::End => out.kind = READ_END,
            ReadStep::Found => out.kind = READ_FOUND,
        }
        out
    }
}

/// The reader at `reader`.
///
/// # Safety
///
/// `reader` must be null or point to a reader this process alone uses.
unsafe fn reader_at<'a>(reader: *mut Reader) -> Result<&'a mut Reader> {
    ensure!(reader.is_aligned(), "a reader not aligned to 8");
    // SAFETY: the caller's contract; any bytes are a reader, whose calls
    // check them.
    unsafe { reader.as_mut() }.context("a null reader")
}

/// `len` entries at `entries`, a page or a list, empty when there are
/// none.
///
/// # Safety
///
/// `entries` must point to `len` initialized entries, valid and unchanged
/// for `'a`, when `len` is positive.
unsafe fn page_at<'a>(entries: *const Entry, len: usize) -> Result<&'a [Entry]> {
    ensure!(len == 0 || entries.is_aligned(), "a page not aligned to 8");
    // SAFETY: the caller's contract.
    unsafe { values(entries, len, "page") }
}

/// Write `step` to `*out`, checked for null and alignment.
///
/// # Safety
///
/// `out` must be null or writable.
unsafe fn put_step(out: *mut SpillReadStep, step: ReadStep) -> Result<()> {
    // SAFETY: the caller's contract.
    unsafe { output(out, "step")?.write(SpillReadStep::from_step(step)) };
    Ok(())
}

fn read_config(
    npartitions: c_int,
    level: u32,
    partition: c_int,
    fingerprint: u64,
    bounds: Bounds,
) -> Result<ReadConfig> {
    Ok(ReadConfig {
        partitions: u32::try_from(npartitions).context("negative partitions")?,
        level,
        partition: u32::try_from(partition).context("a negative partition")?,
        fingerprint,
        bounds,
    })
}

/// `tess_spill_reader_file`: make `*reader` a reader of `partition` of a
/// finished set's file of `file_len` bytes, the caller's own or another
/// participant's ([`Reader::file`]), its blocks within `max_chunk` and
/// `max_values` as [`tess_spill_set_init`] takes them.
///
/// # Safety
///
/// As for [`tess_spill_set_init`].
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments, reason = "the scalars of a C entry point")]
pub unsafe extern "C" fn tess_spill_reader_file(
    reader: *mut Reader,
    npartitions: c_int,
    level: u32,
    partition: c_int,
    fingerprint: u64,
    max_chunk: u64,
    max_values: u64,
    file_len: u64,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let out = reader_at(reader)?;
            let bounds = Bounds {
                chunk: max_chunk,
                values: max_values,
            };
            let config = read_config(npartitions, level, partition, fingerprint, bounds)?;
            *out = Reader::file(config, file_len)?;
            Ok(())
        })
    }
}

/// `tess_spill_reader_open`: a step of opening another participant's
/// file into `*step` ([`Reader::open`]); the page holds what the last
/// step asked for.
///
/// # Safety
///
/// `reader` as for [`tess_spill_set_init`]; `page` must point to
/// `page_len` entries; `step` must be writable; `status` as for every
/// entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_spill_reader_open(
    reader: *mut Reader,
    page: *const Entry,
    page_len: usize,
    step: *mut SpillReadStep,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let reader = reader_at(reader)?;
            let found = reader.open(page_at(page, page_len)?)?;
            put_step(step, found)
        })
    }
}

/// `tess_spill_reader_next`: a step of reading the next block into
/// `*step` ([`Reader::next`]); the page (the list, of the own set) and
/// the block buffer hold what the last step asked for.
///
/// # Safety
///
/// As for [`tess_spill_reader_open`], and `block` must point to
/// `block_len` bytes when `block_len` is positive.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_spill_reader_next(
    reader: *mut Reader,
    page: *const Entry,
    page_len: usize,
    block: *const u8,
    block_len: usize,
    step: *mut SpillReadStep,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let reader = reader_at(reader)?;
            let found =
                reader.next(page_at(page, page_len)?, values(block, block_len, "block")?)?;
            put_step(step, found)
        })
    }
}

/// `tess_spill_reader_body`: the body of the block whose header the last
/// step gave, from the block buffer into the `len` bytes at `body`
/// ([`Reader::body`]).
///
/// # Safety
///
/// `reader` as for [`tess_spill_set_init`]; `block` must point to
/// `block_len` bytes, and `body` to `len` writable bytes, when they are
/// positive, not overlapping; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_spill_reader_body(
    reader: *mut Reader,
    block: *const u8,
    block_len: usize,
    body: *mut u8,
    len: usize,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let reader = reader_at(reader)?;
            Ok(reader.body(
                values(block, block_len, "block")?,
                slots(body, len, "body")?,
            )?)
        })
    }
}

/// `tess_spill_reader_seek`: a step of going to the block a write placed
/// at byte `offset` of the file, into `*step` ([`Reader::seek`]).
///
/// # Safety
///
/// As for [`tess_spill_reader_open`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_spill_reader_seek(
    reader: *mut Reader,
    page: *const Entry,
    page_len: usize,
    offset: u64,
    step: *mut SpillReadStep,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let reader = reader_at(reader)?;
            let found = reader.seek(page_at(page, page_len)?, offset)?;
            put_step(step, found)
        })
    }
}
