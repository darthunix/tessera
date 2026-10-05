//! The spill entry points, declared in `include/tessera/spill.h`: the
//! headers of spilled blocks ([`tessera_spill`]), which carry a table's
//! layout fingerprint (`tess_table_fingerprint`).

use std::ffi::{CStr, c_int};
use std::mem::MaybeUninit;
use std::slice;

use anyhow::{Context, Result, bail, ensure};
use tessera_kernels::spill_columns::{self, ColumnChunks, RowValues, Value};
use tessera_kernels::table::{Partitions, PayloadColumns};
use tessera_spill::{BlockHeader, BlockKind, HEADER_SIZE, columns, plan};

use super::column::DatumColumn;
use super::mask::Mask;
use super::status::{Code, Status, guard};
use super::table::{slots, values};
use super::varlena::varlena_size;

/// `TessSpillHeader`: what a spilled block holds.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct SpillHeader {
    /// A `TessSpillKind`: 1 records, 2 values, 3 columns.
    pub kind: u32,
    pub number: u32,
    pub partition: u32,
    pub level: u32,
    pub fingerprint: u64,
    /// Bytes of the chunk a reader gets.
    pub len: u64,
    /// Bytes on disk of a body stored packed, 0 for a body as it is.
    pub packed: u32,
}

impl SpillHeader {
    fn to_block(self) -> anyhow::Result<BlockHeader> {
        let kind = match self.kind {
            1 => BlockKind::Records,
            2 => BlockKind::Values,
            3 => BlockKind::Columns,
            other => bail!("a spilled block of unknown kind {other}"),
        };
        Ok(BlockHeader {
            kind,
            number: self.number,
            partition: self.partition,
            level: self.level,
            fingerprint: self.fingerprint,
            len: self.len,
            packed: self.packed,
        })
    }

    fn from_block(block: BlockHeader) -> Self {
        Self {
            kind: block.kind as u32,
            number: block.number,
            partition: block.partition,
            level: block.level,
            fingerprint: block.fingerprint,
            len: block.len,
            packed: block.packed,
        }
    }
}

/// `tess_spill_header_size`: bytes of a spilled block's header.
#[unsafe(no_mangle)]
pub extern "C" fn tess_spill_header_size() -> usize {
    HEADER_SIZE
}

/// `tess_spill_header_write`: lay out the header of a block in the first
/// `tess_spill_header_size` bytes at `out`, checked as a reader would
/// with `max_len` the most body bytes it accepts.
///
/// # Safety
///
/// `out` must be valid for writes of `len` bytes and `header` point to a
/// header; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_spill_header_write(
    out: *mut u8,
    len: usize,
    header: *const SpillHeader,
    max_len: u64,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let header = header.as_ref().context("a null spill header")?.to_block()?;
            let out = slice::from_raw_parts_mut(
                (!out.is_null()).then_some(out).context("a null buffer")?,
                len,
            );
            header.write(out, max_len)
        })
    }
}

/// `tess_spill_header_read`: read and check the header at `bytes`: its
/// magic, version, kind, the set's `fingerprint`, a body of at most
/// `max_len` bytes, the packed length its kind allows and the level.
///
/// # Safety
///
/// `bytes` must be valid for reads of `len` bytes and `header` writable;
/// `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_spill_header_read(
    bytes: *const u8,
    len: usize,
    fingerprint: u64,
    max_len: u64,
    header: *mut SpillHeader,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let bytes = slice::from_raw_parts(
                (!bytes.is_null())
                    .then_some(bytes)
                    .context("a null buffer")?,
                len,
            );
            let block = BlockHeader::read(bytes, fingerprint, max_len)?;
            *header.as_mut().context("a null spill header")? = SpillHeader::from_block(block);
            Ok(())
        })
    }
}

/// `tess_spill_pack`: pack the `len` bytes of a chunk of records at `chunk`
/// into `out`, of `capacity` bytes: the packed length into `packed`, or 0
/// when the chunk is not one packing takes or would not get shorter.
///
/// # Safety
///
/// `chunk` must be valid for reads of `len` bytes, `out` for writes of
/// `capacity` bytes, and `packed` writable; `status` as for every entry
/// point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_spill_pack(
    chunk: *const u8,
    len: usize,
    out: *mut u8,
    capacity: usize,
    packed: *mut usize,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let chunk = slice::from_raw_parts(
                (!chunk.is_null())
                    .then_some(chunk)
                    .context("a null chunk")?,
                len,
            );
            let out = slice::from_raw_parts_mut(
                (!out.is_null()).then_some(out).context("a null buffer")?,
                capacity,
            );
            *packed.as_mut().context("a null length")? =
                tessera_spill::pack(chunk, out).unwrap_or(0);
            Ok(())
        })
    }
}

/// `tess_spill_unpack`: unpack the `len` bytes `tess_spill_pack` made at
/// `packed` into the chunk of `chunk_len` bytes at `chunk`.
///
/// # Safety
///
/// `packed` must be valid for reads of `len` bytes and `chunk` for writes
/// of `chunk_len` bytes; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_spill_unpack(
    packed: *const u8,
    len: usize,
    chunk: *mut u8,
    chunk_len: usize,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let packed = slice::from_raw_parts(
                (!packed.is_null())
                    .then_some(packed)
                    .context("a null packed body")?,
                len,
            );
            let chunk = slice::from_raw_parts_mut(
                (!chunk.is_null())
                    .then_some(chunk)
                    .context("a null chunk")?,
                chunk_len,
            );
            tessera_spill::unpack(packed, chunk)
        })
    }
}

/// `TessSpillColumnsField`: the bytes of a chunk of columns' header.
pub const COLUMNS_HEADER_SIZE: c_int = 0;
/// Where a chunk's row count lies.
pub const COLUMNS_ROWS_OFFSET: c_int = 1;
/// Where its capacity lies.
pub const COLUMNS_CAPACITY_OFFSET: c_int = 2;
/// Where its count of stored words lies.
pub const COLUMNS_WORDS_OFFSET: c_int = 3;

/// `tess_spill_columns_layout`: the size or offset of a
/// `TessSpillColumnsField` of a chunk of columns' header, for the C
/// side's inline readers to check; 0 for another code.
#[unsafe(no_mangle)]
pub extern "C" fn tess_spill_columns_layout(what: c_int) -> usize {
    match what {
        COLUMNS_HEADER_SIZE => columns::HEADER,
        COLUMNS_ROWS_OFFSET => columns::ROWS_AT,
        COLUMNS_CAPACITY_OFFSET => columns::CAPACITY_AT,
        COLUMNS_WORDS_OFFSET => columns::WORDS_AT,
        _ => 0,
    }
}

/// `TessSpillColumnsShape`: a chunk's lanes of NULL bits.
pub const COLUMNS_SHAPE_NULL_LANES: c_int = 0;
/// The most its packed form takes past its bytes.
pub const COLUMNS_SHAPE_SLACK: c_int = 1;

/// `tess_spill_columns_shape`: a `TessSpillColumnsShape` count of a chunk
/// of columns of `words` stored words, which the C side's inline formulas
/// repeat (`tess_spill_columns_null_lanes`, `TESS_SPILL_COLUMNS_SLACK`) and
/// its tests compare; 0 for another code.
#[unsafe(no_mangle)]
pub extern "C" fn tess_spill_columns_shape(words: u32, what: c_int) -> usize {
    let words = words as usize;
    match what {
        COLUMNS_SHAPE_NULL_LANES => columns::null_lanes(words),
        COLUMNS_SHAPE_SLACK => columns::pack_slack(words),
        _ => 0,
    }
}

/// `tess_spill_partitions`: the partitions of a level of a spill
/// (tessera-spill, `plan::partitions`), into `partitions`.
///
/// # Safety
///
/// `partitions` must be null or writable; `status` as for every entry
/// point.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments, reason = "the scalars of a C entry point")]
pub unsafe extern "C" fn tess_spill_partitions(
    expected: f64,
    limit: usize,
    reserve: usize,
    shift: u32,
    min_partitions: u32,
    max_partitions: u32,
    at_least: u32,
    partitions: *mut u32,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let level = plan::Level {
                expected,
                limit,
                reserve,
                shift,
                min_partitions,
                max_partitions,
                at_least,
            };
            *partitions.as_mut().context("a null count of partitions")? = plan::partitions(&level)?;
            Ok(())
        })
    }
}

/// `tess_spill_chunk_len`: the length of a level's chunks (tessera-spill,
/// `plan::chunk_len`), into `chunk_len`.
///
/// # Safety
///
/// `chunk_len` must be null or writable; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_spill_chunk_len(
    limit: usize,
    partitions: u32,
    share: usize,
    min_chunk: usize,
    max_chunk: usize,
    chunk_len: *mut usize,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            *chunk_len.as_mut().context("a null chunk length")? =
                plan::chunk_len(limit, partitions, share, min_chunk, max_chunk)?;
            Ok(())
        })
    }
}

/// `tess_spill_columns_init`: make the `len` bytes at `chunk` an empty
/// chunk of columns of `words` stored words; its capacity into `capacity`.
///
/// # Safety
///
/// `chunk` must be valid for writes of `len` bytes and `capacity`
/// writable; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_spill_columns_init(
    chunk: *mut u8,
    len: usize,
    words: c_int,
    capacity: *mut usize,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let chunk = slice::from_raw_parts_mut(
                (!chunk.is_null())
                    .then_some(chunk)
                    .context("a null chunk")?,
                len,
            );
            let words = usize::try_from(words).context("a negative word count")?;
            *capacity.as_mut().context("a null capacity")? = columns::init(chunk, words)?;
            Ok(())
        })
    }
}

/// The chunks of columns of a side, by number, as C holds them.
struct RawChunks<'a> {
    bases: &'a [*mut u8],
    lens: &'a [usize],
}

impl ColumnChunks for RawChunks<'_> {
    fn chunk(&mut self, index: usize) -> Result<&mut [u8]> {
        ensure!(
            index < self.bases.len(),
            "chunk {index} of {} chunks of columns",
            self.bases.len()
        );
        let base = self.bases[index];
        ensure!(!base.is_null(), "chunk {index} of columns is gone");
        // SAFETY: the caller's contract of the entry point: every base is
        // valid for its length and nothing else accesses it during the
        // call; the borrow is the only one while it lasts.
        Ok(unsafe { slice::from_raw_parts_mut(base, self.lens[index]) })
    }
}

/// `tess_spill_columns_append_partitioned`: append the rows of `pending`
/// to the chunks of columns of their partitions, as
/// `tess_table_append_partitioned_columns` appends records.
///
/// # Safety
///
/// `bases` and `lens` must point to `nchunks` chunks of columns and their
/// lengths, each valid for writes and accessed by nothing else during the
/// call; `partition_chunks` to `npartitions` numbers, `hashes` to a hash
/// per row of `pending`, a valid mask, `columns` to `ncolumns` columns of
/// its rows, `offsets` to a slot per row and `rows` to `npartitions`
/// counts, writable; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_spill_columns_append_partitioned(
    bases: *const *mut u8,
    lens: *const usize,
    nchunks: c_int,
    partition_chunks: *const u32,
    npartitions: c_int,
    shift: u32,
    hashes: *const u32,
    ncolumns: c_int,
    columns_in: *const DatumColumn,
    pending: *mut Mask,
    offsets: *mut u32,
    rows: *mut u64,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let nchunks = usize::try_from(nchunks).context("a negative chunk count")?;
            let npartitions = usize::try_from(npartitions).context("a negative partition count")?;
            let mut chunks = RawChunks {
                bases: values(bases, nchunks, "chunk bases")?,
                lens: values(lens, nchunks, "chunk lengths")?,
            };
            let partition_chunks = values(partition_chunks, npartitions, "partition chunks")?;
            let mut pending = pending.as_mut().context("a null pending mask")?.mask()?;
            let nrows = pending.as_view().nrows();
            let hashes = values(hashes, nrows, "hashes")?;
            let offsets = slots(offsets, nrows, "offsets")?;
            let rows = slots(rows, npartitions, "partition row counts")?;
            let ncolumns = usize::try_from(ncolumns).context("a negative column count")?;
            ensure!(
                ncolumns <= 64,
                "a partitioned chunk of columns keeps up to 64 words, not {ncolumns}"
            );
            let given = values(columns_in, ncolumns, "columns")?;
            // Only the columns given are set: a batch of a few rows would
            // otherwise pay for clearing 64 slices of each kind.
            let mut words = [const { MaybeUninit::<&[u64]>::uninit() }; 64];
            let mut flags = [const { MaybeUninit::<&[bool]>::uninit() }; 64];
            for (index, column) in given.iter().enumerate() {
                ensure!(
                    usize::try_from(column.nrows).ok() == Some(nrows),
                    "column {index} has {} rows, not {nrows}",
                    column.nrows
                );
                words[index].write(values(column.values, nrows, "column values")?);
                flags[index].write(values(column.isnull, nrows, "column NULL flags")?);
            }
            // SAFETY: the loop above initialized the first `ncolumns`
            // slots of each array, and `ncolumns` is at most 64.
            let (words, flags) = (
                &*(&raw const words[..ncolumns] as *const [&[u64]]),
                &*(&raw const flags[..ncolumns] as *const [&[bool]]),
            );
            let payload = PayloadColumns::new(words, flags, nrows)?;
            spill_columns::append_partitioned(
                &mut chunks,
                &Partitions {
                    shift,
                    chunks: partition_chunks,
                },
                hashes,
                &payload,
                &mut pending,
                offsets,
                rows,
            )
            .map(drop)
        })
    }
}

/// A batch's Datum columns as a chunk of columns stores them: a by-value
/// column's Datum, a by-reference one's bytes as `datumGetSize` counts
/// them.
struct DatumRows<'a> {
    columns: &'a [DatumColumn],
    nrows: usize,
    byvals: &'a [bool],
    typlens: &'a [i16],
    by_value: bool,
}

impl RowValues for DatumRows<'_> {
    fn ncolumns(&self) -> usize {
        self.columns.len()
    }

    fn by_value(&self) -> bool {
        self.by_value
    }

    // Both passes of an append over a row take it: called, it was a fifth
    // of a gather's leader (macOS sample of a sort's merge of text).
    #[inline(always)]
    fn value(&self, column: usize, row: usize) -> Result<Value<'_>> {
        let given = &self.columns[column];
        ensure!(row < self.nrows, "row {row} past the {} rows", self.nrows);
        // SAFETY: the entry point checked each column's arrays non-null
        // and of the rows' count, and its caller made them so.
        let (null, datum) = unsafe { (*given.isnull.add(row), *given.values.add(row)) };
        if null {
            return Ok(Value::Null);
        }
        if self.byvals[column] {
            return Ok(Value::Word(datum));
        }
        // SAFETY: the entry point's contract: a by-reference column's
        // non-NULL Datum points to a whole value of its type's length.
        Ok(Value::Bytes(unsafe {
            datum_bytes(datum, self.typlens[column])?
        }))
    }
}

/// The bytes of a by-reference Datum, as `datumGetSize` counts them: a
/// fixed length's, a varlena's whole size (an external pointer's own), a C
/// string's with its terminator.
///
/// # Safety
///
/// `datum` must point to a whole value of a type of length `typlen`,
/// valid for `'a`.
#[inline]
unsafe fn datum_bytes<'a>(datum: u64, typlen: i16) -> Result<&'a [u8]> {
    let pointer = datum as usize as *const u8;
    ensure!(!pointer.is_null(), "a null by-reference value");
    // SAFETY: the caller's contract.
    unsafe {
        let len = match typlen {
            len if len > 0 => len as usize,
            -1 => varlena_size(pointer)?,
            -2 => CStr::from_ptr(pointer.cast()).to_bytes_with_nul().len(),
            other => bail!("a type of length {other}"),
        };
        Ok(slice::from_raw_parts(pointer, len))
    }
}

/// `tess_spill_columns_append`: the selected rows of `ncolumns` Datum
/// columns appended to the chunk of columns at `chunk`, their
/// by-reference values copied into `values`
/// ([`spill_columns::append`]).
///
/// # Safety
///
/// `chunk` must point to `len` writable bytes of a chunk of columns;
/// `columns`, `byvals` and `typlens` to `ncolumns` entries each, every
/// column of `rows`' row count, a non-NULL by-reference Datum pointing to
/// a whole value of its type's length; `rows` to a valid mask; `values`
/// to `values_len` writable bytes (null when 0); `values_used`,
/// `appended` and `need` writable; none of the written memory accessed by
/// anything else for the call; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_spill_columns_append(
    chunk: *mut u8,
    len: usize,
    ncolumns: c_int,
    columns_in: *const DatumColumn,
    byvals: *const bool,
    typlens: *const i16,
    rows: *mut Mask,
    values_out: *mut u8,
    values_len: usize,
    values_used: *mut usize,
    appended: *mut c_int,
    need: *mut usize,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            ensure!(!chunk.is_null(), "a null chunk of columns");
            let chunk = slice::from_raw_parts_mut(chunk, len);
            let mut rows = rows.as_mut().context("a null row mask")?.mask()?;
            let nrows = rows.as_view().nrows();
            let ncolumns = usize::try_from(ncolumns).context("a negative column count")?;
            let given = values(columns_in, ncolumns, "columns")?;
            let byvals = values(byvals, ncolumns, "by-value flags")?;
            let typlens = values(typlens, ncolumns, "type lengths")?;
            for (index, column) in given.iter().enumerate() {
                ensure!(
                    usize::try_from(column.nrows).ok() == Some(nrows)
                        && (nrows == 0 || (!column.values.is_null() && !column.isnull.is_null())),
                    "column {index} of {} rows does not hold {nrows}",
                    column.nrows
                );
            }
            let source = DatumRows {
                columns: given,
                nrows,
                byvals,
                typlens,
                by_value: byvals.iter().all(|&byval| byval),
            };
            let values_out = if values_len == 0 {
                &mut [][..]
            } else {
                ensure!(!values_out.is_null(), "null values");
                slice::from_raw_parts_mut(values_out, values_len)
            };
            let used = values_used
                .as_mut()
                .context("a null count of values used")?;
            let done = spill_columns::append(chunk, &source, &mut rows, values_out, used)?;
            *appended.as_mut().context("a null count appended")? =
                c_int::try_from(done.rows).context("rows past an int")?;
            *need.as_mut().context("a null count of bytes")? = done.need;
            Ok(())
        })
    }
}

/// `tess_spill_columns_pack`: pack the chunk of columns at `chunk` into
/// `out`, of `capacity` bytes (the chunk's length and
/// `TESS_SPILL_COLUMNS_SLACK` suffice): the packed length into `packed`
/// and the length of the chunk it unpacks into into `unpacked`.
///
/// # Safety
///
/// `chunk` must be valid for reads of `len` bytes, `out` for writes of
/// `capacity` bytes, and `packed` and `unpacked` writable; `status` as
/// for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_spill_columns_pack(
    chunk: *const u8,
    len: usize,
    out: *mut u8,
    capacity: usize,
    packed: *mut usize,
    unpacked: *mut usize,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let chunk = slice::from_raw_parts(
                (!chunk.is_null())
                    .then_some(chunk)
                    .context("a null chunk")?,
                len,
            );
            let out = slice::from_raw_parts_mut(
                (!out.is_null()).then_some(out).context("a null buffer")?,
                capacity,
            );
            let (bytes, len) = columns::pack(chunk, out)?;
            *packed.as_mut().context("a null length")? = bytes;
            *unpacked.as_mut().context("a null length")? = len;
            Ok(())
        })
    }
}

/// `tess_spill_columns_unpack`: unpack the `len` bytes
/// `tess_spill_columns_pack` made at `packed` into the chunk of
/// `chunk_len` bytes at `chunk`, the length it returned.
///
/// # Safety
///
/// `packed` must be valid for reads of `len` bytes and `chunk` for writes
/// of `chunk_len` bytes; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_spill_columns_unpack(
    packed: *const u8,
    len: usize,
    chunk: *mut u8,
    chunk_len: usize,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let packed = slice::from_raw_parts(
                (!packed.is_null())
                    .then_some(packed)
                    .context("a null packed body")?,
                len,
            );
            let chunk = slice::from_raw_parts_mut(
                (!chunk.is_null())
                    .then_some(chunk)
                    .context("a null chunk")?,
                chunk_len,
            );
            columns::unpack(packed, chunk)
        })
    }
}
