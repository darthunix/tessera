//! The spill entry points, declared in `include/tessera/spill.h`: the
//! headers of spilled blocks ([`relspill`]), which carry a table's
//! layout fingerprint (`tess_table_fingerprint`).

use std::cell::Cell;
use std::ffi::{CStr, c_int};
use std::mem::MaybeUninit;
use std::slice;

use anyhow::{Context, Result, bail, ensure};
use relspill::{BlockHeader, BlockKind, Bounds, HEADER_SIZE, columns, plan};
use tessera_kernels::spill_columns::{self, ColumnChunks, RowValues, Value, ValueEnds};
use tessera_kernels::table::{Partitions, PayloadColumns};

use super::column::DatumColumn;
use super::mask::Mask;
use super::status::{Code, Status, guard};
use super::table::{output, slots, values};
use super::varlena::{ondisk_body, varlena_size};

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

    pub(super) fn from_block(block: BlockHeader) -> Self {
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
/// with `max_len` the most body bytes it accepts, of every kind.
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
            Ok(header.write(out, Bounds::all(max_len))?)
        })
    }
}

/// `tess_spill_header_read`: read and check the header at `bytes`: its
/// magic, version, kind, the set's `fingerprint`, a body of at most
/// `max_len` bytes of every kind, the packed length its kind allows and the level.
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
            let header = output(header, "spill header")?;
            let bytes = slice::from_raw_parts(
                (!bytes.is_null())
                    .then_some(bytes)
                    .context("a null buffer")?,
                len,
            );
            let block = BlockHeader::read(bytes, fingerprint, Bounds::all(max_len))?;
            header.write(SpillHeader::from_block(block));
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
            let packed = output(packed, "length")?;
            packed.write(relspill::pack(chunk, out).unwrap_or(0));
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
            Ok(relspill::unpack(packed, chunk)?)
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

/// `TessSpillShape`: what every level of a node's spill shares; see
/// [`plan::Shape`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct SpillShape {
    pub reserve: usize,
    pub share: usize,
    pub min_chunk: usize,
    pub max_chunk: usize,
    pub record: usize,
    pub min_partitions: u32,
    pub max_partitions: u32,
}

/// `TessSpillLevel`: a level about to be made; see [`plan::Level`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct SpillLevel {
    pub expected: f64,
    pub limit: usize,
    pub shift: u32,
    pub at_least: u32,
    pub partitions: u32,
}

/// `TessSpillPlan`: the plan of a level; see [`plan::Plan`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SpillPlan {
    pub chunk_len: usize,
    pub words: usize,
    pub partitions: u32,
    pub shift: u32,
    pub below: u32,
    pub deeper: bool,
}

/// `tess_spill_plan`: the plan of a level of a node's spill (relspill,
/// [`plan::plan`]), into `plan`.
///
/// # Safety
///
/// `shape` and `level` must be null or readable, `plan` null or writable;
/// `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_spill_plan(
    shape: *const SpillShape,
    level: *const SpillLevel,
    plan: *mut SpillPlan,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let shape = shape.as_ref().context("a null shape of a spill")?;
            let level = level.as_ref().context("a null level of a spill")?;
            let out = output(plan, "plan of a level")?;
            let got = plan::plan(
                &plan::Shape {
                    reserve: shape.reserve,
                    min_partitions: shape.min_partitions,
                    max_partitions: shape.max_partitions,
                    share: shape.share,
                    min_chunk: shape.min_chunk,
                    max_chunk: shape.max_chunk,
                    record: shape.record,
                },
                &plan::Level {
                    expected: level.expected,
                    limit: level.limit,
                    shift: level.shift,
                    at_least: level.at_least,
                    partitions: level.partitions,
                },
            )?;
            out.write(SpillPlan {
                chunk_len: got.chunk_len,
                words: got.words,
                partitions: got.partitions,
                shift: got.shift,
                below: got.below,
                deeper: got.deeper,
            });
            Ok(())
        })
    }
}

/// `tess_spill_expected`: the bytes the first level of a spill expects
/// (relspill, [`plan::expected`]).
#[unsafe(no_mangle)]
pub extern "C" fn tess_spill_expected(
    held: f64,
    rows: f64,
    estimate: f64,
    participants: u32,
) -> f64 {
    plan::expected(held, rows, estimate, participants)
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
            let capacity = output(capacity, "capacity")?;
            capacity.write(columns::init(chunk, words)?);
            Ok(())
        })
    }
}

/// The chunks of columns of a side, by number, as C holds them.
struct RawChunks<'a> {
    bases: &'a [*mut u8],
    lens: &'a [usize],
    /// Why `chunk` last gave none, which the error of the spill names
    /// only by its rule.
    failure: Option<anyhow::Error>,
}

impl ColumnChunks for RawChunks<'_> {
    /// `None` for a chunk past the chunks, or one that is gone, with why.
    fn chunk(&mut self, index: usize) -> Option<&mut [u8]> {
        let Some(&base) = self.bases.get(index) else {
            return self.refuse(anyhow::anyhow!(
                "chunk {index} of {} chunks of columns",
                self.bases.len()
            ));
        };
        if base.is_null() {
            return self.refuse(anyhow::anyhow!("chunk {index} of columns is gone"));
        }
        // SAFETY: the caller's contract of the entry point: every base is
        // valid for its length and nothing else accesses it during the
        // call; the borrow is the only one while it lasts.
        Some(unsafe { slice::from_raw_parts_mut(base, self.lens[index]) })
    }
}

impl RawChunks<'_> {
    /// Keep why a chunk cannot be given, and give none.
    #[cold]
    #[inline(never)]
    fn refuse<'c>(&mut self, reason: anyhow::Error) -> Option<&'c mut [u8]> {
        self.failure = Some(reason);
        None
    }

    /// The error of an append over these chunks: the reason `chunk` kept,
    /// when it gave none, as the spill stops there with
    /// [`relspill::Rule::ChunkRead`]; any other error of the spill as it is.
    #[cold]
    #[inline(never)]
    fn reason(&mut self, error: relspill::Error) -> anyhow::Error {
        self.failure.take().unwrap_or_else(|| error.into())
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
                failure: None,
            };
            let partition_chunks = values(partition_chunks, npartitions, "partition chunks")?;
            let mut pending = pending.as_mut().context("a null pending mask")?.mask()?;
            let nrows = pending.as_view().nrows();
            let hashes = values(hashes, nrows, "hashes")?;
            let offsets = slots(offsets, nrows, "offsets")?;
            let rows = slots(rows, npartitions, "partition row counts")?;
            let ncolumns = usize::try_from(ncolumns).context("a negative column count")?;
            let given = values(columns_in, ncolumns, "columns")?;
            let slices = |index: usize, column: &DatumColumn| -> Result<(&[u64], &[bool])> {
                ensure!(
                    usize::try_from(column.nrows).ok() == Some(nrows),
                    "column {index} has {} rows, not {nrows}",
                    column.nrows
                );
                Ok((
                    values(column.values, nrows, "column values")?,
                    values(column.isnull, nrows, "column NULL flags")?,
                ))
            };
            // Up to 64 columns, only the columns given are set: a batch of
            // a few rows would otherwise pay for clearing 64 slices of each
            // kind. A wider row, which is rare, has its slices on the heap.
            let mut words = [const { MaybeUninit::<&[u64]>::uninit() }; 64];
            let mut flags = [const { MaybeUninit::<&[bool]>::uninit() }; 64];
            let (mut wide_words, mut wide_flags) = (Vec::new(), Vec::new());
            let (words, flags): (&[&[u64]], &[&[bool]]) = if ncolumns <= 64 {
                for (index, column) in given.iter().enumerate() {
                    let (column_words, column_flags) = slices(index, column)?;
                    words[index].write(column_words);
                    flags[index].write(column_flags);
                }
                // SAFETY: the loop above initialized the first `ncolumns`
                // slots of each array, and `ncolumns` is at most 64.
                (
                    &*(&raw const words[..ncolumns] as *const [&[u64]]),
                    &*(&raw const flags[..ncolumns] as *const [&[bool]]),
                )
            } else {
                for (index, column) in given.iter().enumerate() {
                    let (column_words, column_flags) = slices(index, column)?;
                    wide_words.push(column_words);
                    wide_flags.push(column_flags);
                }
                (&wide_words, &wide_flags)
            };
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
            .map_err(|error| chunks.reason(error))?;
            Ok(())
        })
    }
}

/// A batch's Datum columns as a chunk of columns stores them: a by-value
/// column's Datum, a by-reference one's bytes as `datumGetSize` counts
/// them; an expanded object is refused, its caller flattens it first.
struct DatumRows<'a> {
    columns: &'a [DatumColumn],
    nrows: usize,
    byvals: &'a [bool],
    typlens: &'a [i16],
    by_value: bool,
    /// Why `value` last refused a value, which the error of the spill
    /// names only by its rule. A cell outside, not a field: a cell in the
    /// rows would take from `&DatumRows` the promise that nothing else
    /// writes it, and the appends' loops would read its fields again
    /// after every store.
    failure: &'a Cell<Option<anyhow::Error>>,
}

impl RowValues for DatumRows<'_> {
    fn ncolumns(&self) -> usize {
        self.columns.len()
    }

    fn by_value(&self) -> bool {
        self.by_value
    }

    // Both passes of an append over a row take it: called, it was a fifth
    // of a gather's leader (macOS sample of a sort's merge of text). Why a
    // value cannot be told is kept out of line.
    #[inline(always)]
    fn value(&self, column: usize, row: usize) -> Option<Value<'_>> {
        let given = &self.columns[column];
        if row >= self.nrows {
            self.refuse(anyhow::anyhow!("row {row} past the {} rows", self.nrows));
            return None;
        }
        // SAFETY: the entry point checked each column's arrays non-null
        // and of the rows' count, and its caller made them so.
        let (null, datum) = unsafe { (*given.isnull.add(row), *given.values.add(row)) };
        if null {
            return Some(Value::Null);
        }
        if self.byvals[column] {
            return Some(Value::Word(datum));
        }
        // SAFETY: the entry point's contract: a by-reference column's
        // non-NULL Datum points to a whole value of its type's length.
        match unsafe { datum_bytes(datum, self.typlens[column]) } {
            Ok(bytes) => Some(Value::Bytes(bytes)),
            Err(reason) => {
                self.refuse(reason);
                None
            }
        }
    }
}

impl DatumRows<'_> {
    /// Keep why a value cannot be given. It returns nothing: a value of
    /// 24 bytes would come back through memory, and the appends' loops
    /// would keep every value there.
    #[cold]
    #[inline(never)]
    fn refuse(&self, reason: anyhow::Error) {
        self.failure.set(Some(reason));
    }
    /// The error of a call over these rows: the reason `value` kept, when
    /// it gave none, as the spill stops there with
    /// [`relspill::Rule::ValueRead`]; any other error of the spill as it is.
    #[cold]
    #[inline(never)]
    fn reason(&self, error: relspill::Error) -> anyhow::Error {
        self.failure.take().unwrap_or_else(|| error.into())
    }
}

/// The bytes of a by-reference Datum, as `datumGetSize` counts them: a
/// fixed length's, a varlena's whole size (an external pointer's own, but
/// for an expanded object's, which is refused), a C string's with its
/// terminator.
///
/// # Safety
///
/// `datum` must point to a whole value of a type of length `typlen`,
/// valid for `'a`.
#[inline]
unsafe fn datum_bytes<'a>(datum: u64, typlen: i16) -> Result<&'a [u8]> {
    // A Datum is an integer that holds a pointer C made: its provenance
    // is exposed, which Miri takes on trust and says so.
    let pointer = core::ptr::with_exposed_provenance::<u8>(datum as usize);
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
            let failure = Cell::new(None);
            let source = DatumRows {
                columns: given,
                nrows,
                byvals,
                typlens,
                by_value: byvals.iter().all(|&byval| byval),
                failure: &failure,
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
            let appended = output(appended, "count appended")?;
            let need = output(need, "count of bytes")?;
            let done = spill_columns::append(chunk, &source, &mut rows, values_out, used)
                .map_err(|error| source.reason(error))?;
            appended.write(c_int::try_from(done.rows).context("rows past an int")?);
            need.write(done.need);
            Ok(())
        })
    }
}

/// `tess_spill_columns_pair`: whether `header`, read back as block `at`
/// of the pair a chunk of columns is written as (0 its values, 1 its
/// rows), numbered `number`, is that block; block 0 may be the rows of a
/// chunk with no values, the whole pair ([`spill_columns::check_pair`]).
///
/// # Safety
///
/// `header` must be null or readable; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_spill_columns_pair(
    header: *const SpillHeader,
    at: c_int,
    number: u32,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let header = header.as_ref().context("a null header")?.to_block()?;
            let at = usize::try_from(at).context("a negative block of a pair")?;
            spill_columns::check_pair(&header, at, number)?;
            Ok(())
        })
    }
}

/// Where a value of a PostgreSQL type ends in the bytes from its start,
/// `typlen` its type's length: a fixed length within them; a varlena by
/// its header, read by bytes as a damaged one may lie at any byte, a
/// compressed one holding its 8 bytes of header, an external one only a
/// pointer to data on disk; a C string at its NUL. Little-endian headers,
/// as `varlena.rs` reads them. The C of `tess_spill_value_fits` keeps the
/// same rule for the readers that are not moved yet.
pub fn value_ends(typlen: i16, rest: &[u8]) -> bool {
    match typlen {
        len if len > 0 => len as usize <= rest.len(),
        -1 => match rest {
            [] | [0x01] => false,
            // An external value: a 2-byte header, and a tag of data on
            // disk, the body of its pointer after it.
            [0x01, tag, ..] => ondisk_body(*tag).is_some_and(|body| 2 + body <= rest.len()),
            [first, ..] if first & 0x01 == 0x01 => usize::from(first >> 1) <= rest.len(),
            _ => rest.first_chunk::<4>().is_some_and(|header| {
                let header = u32::from_le_bytes(*header);
                let size = (header >> 2) as usize;
                let least = if header & 0x03 == 0x02 { 8 } else { 4 };
                size >= least && size <= rest.len()
            }),
        },
        -2 => rest.contains(&0),
        _ => false,
    }
}

/// The ends of values by their columns' type lengths.
struct TypeEnds<'a> {
    typlens: &'a [i16],
}

impl ValueEnds for TypeEnds<'_> {
    #[inline]
    fn ends(&self, column: usize, rest: &[u8]) -> bool {
        self.typlens
            .get(column)
            .is_some_and(|&typlen| value_ends(typlen, rest))
    }
}

/// `tess_spill_columns_check`: check the chunk of columns at `chunk`, read
/// back with its chunk of `values_len` bytes at `values`, once, in one
/// pass: `words` words, `rows` rows unless -1, and every word of a column
/// not by value of the first `ncolumns` (by `byvals`) of a row not NULL a
/// reference into the values, whose value ends inside them by its type
/// (`typlens`, [`value_ends`]) ([`spill_columns::check`]).
///
/// # Safety
///
/// `chunk` must be valid for reads of `len` bytes, `values` for
/// `values_len` bytes (null when 0), `byvals` and `typlens` for `ncolumns`
/// entries; `status` as for every entry point.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments, reason = "the scalars of a C entry point")]
pub unsafe extern "C" fn tess_spill_columns_check(
    chunk: *const u8,
    len: usize,
    words: c_int,
    rows: i64,
    ncolumns: c_int,
    byvals: *const bool,
    typlens: *const i16,
    values_in: *const u8,
    values_len: usize,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            ensure!(!chunk.is_null(), "a null chunk of columns");
            let chunk = slice::from_raw_parts(chunk, len);
            let words = usize::try_from(words).context("a negative count of words")?;
            let ncolumns = usize::try_from(ncolumns).context("a negative column count")?;
            ensure!(rows >= -1, "a count of rows of {rows}");
            let rows = usize::try_from(rows).ok();
            let byvals = values(byvals, ncolumns, "by-value flags")?;
            let typlens = values(typlens, ncolumns, "type lengths")?;
            let read = if values_len == 0 {
                &[][..]
            } else {
                ensure!(!values_in.is_null(), "null values");
                slice::from_raw_parts(values_in, values_len)
            };
            spill_columns::check(chunk, words, rows, byvals, read, &TypeEnds { typlens })?;
            Ok(())
        })
    }
}

/// `tess_spill_columns_window`: the words of the first `ncolumns` words
/// of the chunk of columns at `chunk` for its `count` rows from `first` on,
/// column `c` as Datums into `out[c]` and whether each is NULL into
/// `isnull[c]` ([`spill_columns::window`]); a word of a by-reference
/// column (not `byvals[c]`) of a row not NULL as a pointer to its value in
/// `values`.
///
/// # Safety
///
/// `chunk` must be valid for reads of `len` bytes, read back and passed
/// by `tess_spill_columns_check` with `values`; `byvals`, `out` and
/// `isnull` must hold `ncolumns` entries, each of `out` and `isnull`
/// valid for writes of `count` entries; `status` as for every entry
/// point.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments, reason = "the scalars of a C entry point")]
pub unsafe extern "C" fn tess_spill_columns_window(
    chunk: *const u8,
    len: usize,
    ncolumns: c_int,
    byvals: *const bool,
    values: *const u8,
    first: usize,
    count: usize,
    out: *const *mut u64,
    isnull: *const *mut bool,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            ensure!(!chunk.is_null(), "a null chunk of columns");
            let chunk = slice::from_raw_parts(chunk, len);
            let ncolumns = usize::try_from(ncolumns).context("a negative column count")?;
            let byvals = self::values(byvals, ncolumns, "by-value flags")?;
            let outs = self::values(out, ncolumns, "windows")?;
            let nulls = self::values(isnull, ncolumns, "windows of NULLs")?;
            // The rows a window asks for are the chunk's, whatever columns.
            let rows = relspill::columns::shape(chunk)?.rows;
            ensure!(
                first <= rows && count <= rows - first,
                "a window of {count} rows from {first} of a chunk of {rows}"
            );
            // The check made every reference one into the values.
            let base = values.expose_provenance() as u64;
            let first_ref = relspill::columns::value_ref(0, 0);
            for column in 0..ncolumns {
                let (out, isnull) = if count == 0 {
                    (&mut [][..], &mut [][..])
                } else {
                    ensure!(
                        !outs[column].is_null() && !nulls[column].is_null(),
                        "a null window of column {column}"
                    );
                    (
                        slice::from_raw_parts_mut(outs[column], count),
                        slice::from_raw_parts_mut(nulls[column], count),
                    )
                };
                spill_columns::window(chunk, column, first, out, isnull)?;
                if !byvals[column] {
                    for (datum, &null) in out.iter_mut().zip(isnull.iter()) {
                        if !null {
                            *datum = base.wrapping_add(datum.wrapping_sub(first_ref));
                        }
                    }
                }
            }
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
            let packed = output(packed, "length")?;
            let unpacked = output(unpacked, "length")?;
            let (bytes, len) = columns::pack(chunk, out)?;
            packed.write(bytes);
            unpacked.write(len);
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
            Ok(columns::unpack(packed, chunk)?)
        })
    }
}
