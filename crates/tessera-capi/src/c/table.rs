//! The table entry points, declared in `include/tessera/table.h`.
//!
//! The table comes as a `TessTableRef` with every call: its index as a
//! pointer and a length, its chunks as this process's bases and lengths.
//! Every call attaches anew: the library keeps nothing between calls. Keys come
//! as `TessTableKey`s, Datum columns read by their kind, and reach the
//! table through [`TableKeys`].

use std::ffi::{c_int, c_uint};
use std::mem::{MaybeUninit, offset_of};
use std::slice;

use anyhow::{Context, Result, bail, ensure};
use tessera_core::ColumnReader;
use tessera_kernels::table::{
    Chunks, Combine, CombineStop, Cursor, FORMAT_VERSION, Fold, HEADER_SIZE, KeyKind, KeySource,
    MAX_KEYS, MAX_PAYLOAD_COLUMNS, MAX_SUMS, Partitions, PayloadColumns, Slot, SumSlot, Table,
    TableConfig, TableMut, UNIT_BITS, VERSION_OFFSET, append_columns_to,
    append_partitioned_columns_to, append_to,
    bloom::SharedFilter,
    index_size, init_chunk, normalize_word, payload_null_words,
    phases::{Participant, SharedCounters},
    split_to,
};

use super::args::reader;
use super::column::DatumColumn;
use super::decimal::{SumColumn, SumInput};
use super::mask::Mask;
use super::status::{Code, Status, guard};
use crate::{DatumInt32Column, DatumInt64Column};

/// `TessTableKey`: one key of a batch.
#[repr(C)]
#[derive(Debug)]
pub struct TableKey {
    /// A `TessTableKeyKind`.
    pub kind: c_uint,
    /// The column, read by the kind.
    pub column: *const DatumColumn,
    /// The column's readiness, or null for a whole column.
    pub prepared: *const Mask,
}

/// `TessTableStats`: the counts of a table.
#[repr(C)]
#[derive(Debug)]
pub struct TableStats {
    /// The size the caller allocated, at least [`TableStats::MIN_SIZE`].
    pub struct_size: usize,
    /// Records inserted.
    pub records: u64,
    /// Buckets of the table.
    pub buckets: u64,
    /// Bytes in use: the header, the records and the buckets.
    pub bytes_used: u64,
    /// Bytes of the region the table was created or grown over.
    pub region_len: u64,
}

impl TableStats {
    /// The size through `region_len` (`TESS_TABLE_STATS_MIN_SIZE`).
    pub const MIN_SIZE: usize = offset_of!(TableStats, region_len) + size_of::<u64>();
}

/// `TessTableRecord`: a record as the table exposes it, pointing into the
/// region.
#[repr(C)]
#[derive(Debug)]
pub struct TableRecord {
    /// The size the caller allocated, at least [`TableRecord::MIN_SIZE`].
    pub struct_size: usize,
    /// The hash it was inserted with.
    pub hash: u32,
    /// Bit `k` set: key `k` is NULL, and its slot holds 0.
    pub null_bits: u32,
    /// One slot per key, in key order.
    pub keys: *const i64,
    /// The payload, of `payload_size` bytes.
    pub payload: *const u8,
    /// Bytes of the payload.
    pub payload_size: usize,
}

impl TableRecord {
    /// The size through `payload_size` (`TESS_TABLE_RECORD_MIN_SIZE`).
    pub const MIN_SIZE: usize = offset_of!(TableRecord, payload_size) + size_of::<usize>();
}

/// `TessTableRef`: a table as this process sees it: its index, and its
/// chunks' bases and lengths.
#[repr(C)]
#[derive(Debug)]
pub struct TableRef {
    /// The index, aligned to 8, or null where only chunks are used.
    pub index: *mut u8,
    /// Bytes of the index.
    pub index_len: usize,
    /// The bases of the chunks, in their numbers' order.
    pub chunks: *const *mut u8,
    /// The lengths of the chunks.
    pub chunk_lens: *const usize,
    /// The number of chunks.
    pub nchunks: c_int,
}

/// The chunks of a table reference.
///
/// # Safety
///
/// `table` must point to a reference whose arrays hold `nchunks` entries
/// each, valid with the chunks they describe for `'a`, as for
/// [`Chunks::new`].
unsafe fn chunks_of<'a>(table: *const TableRef) -> Result<(&'a TableRef, Chunks<'a>)> {
    // SAFETY: the caller's contract.
    let table = unsafe { table.as_ref() }.context("a null table")?;
    let nchunks = usize::try_from(table.nchunks).context("a negative chunk count")?;
    if nchunks == 0 {
        return Ok((table, Chunks::none()));
    }
    ensure!(
        !table.chunks.is_null() && !table.chunk_lens.is_null(),
        "null chunk arrays"
    );
    // SAFETY: the caller's contract.
    let chunks = unsafe {
        Chunks::new(
            slice::from_raw_parts(table.chunks, nchunks),
            slice::from_raw_parts(table.chunk_lens, nchunks),
        )
    }?;
    Ok((table, chunks))
}

/// Attach to a table for the length of a call.
///
/// # Safety
///
/// `table` as for [`chunks_of`], and its index as for [`Table::attach`].
pub(super) unsafe fn attach<'a>(table: *const TableRef) -> Result<Table<'a>> {
    // SAFETY: the caller's contract.
    unsafe {
        let (table, chunks) = chunks_of(table)?;
        Table::attach(table.index, table.index_len, chunks)
    }
}

/// Attach as the one writer of a table for the length of a call.
///
/// # Safety
///
/// As [`attach`], and as [`TableMut::attach_mut`].
unsafe fn attach_mut<'a>(table: *const TableRef) -> Result<TableMut<'a>> {
    // SAFETY: the caller's contract.
    unsafe {
        let (table, chunks) = chunks_of(table)?;
        TableMut::attach_mut(table.index, table.index_len, chunks)
    }
}

/// A key column read by its kind.
#[derive(Debug)]
enum KeyColumn<'a> {
    Int32(DatumInt32Column<'a>),
    Int64(DatumInt64Column<'a>),
}

/// The keys of a batch, as `TessTableKey`s decode to.
///
/// Decoded in place by [`table_keys`] into a value the entry point holds on
/// its stack: the slots for every possible key take a kilobyte, which a
/// value returned from a function would copy on every call. The first
/// `nkeys` slots are initialized.
pub struct TableKeys<'a> {
    columns: [MaybeUninit<KeyColumn<'a>>; MAX_KEYS],
    nkeys: usize,
    nrows: usize,
}

impl TableKeys<'_> {
    /// No keys yet.
    pub(super) fn empty() -> Self {
        Self {
            columns: [const { MaybeUninit::uninit() }; MAX_KEYS],
            nkeys: 0,
            nrows: 0,
        }
    }
}

impl core::fmt::Debug for TableKeys<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TableKeys")
            .field("nkeys", &self.nkeys)
            .field("nrows", &self.nrows)
            .finish_non_exhaustive()
    }
}

impl KeySource for TableKeys<'_> {
    fn nkeys(&self) -> usize {
        self.nkeys
    }

    fn nrows(&self) -> usize {
        self.nrows
    }

    fn word(&self, key: usize, index: usize, selected: u64, out: &mut [i64; 64]) -> Result<u64> {
        assert!(key < self.nkeys, "a key below nkeys");
        // SAFETY: the first `nkeys` slots are initialized.
        match unsafe { self.columns[key].assume_init_ref() } {
            KeyColumn::Int32(column) => normalize_word(column, index, selected, out),
            KeyColumn::Int64(column) => normalize_word(column, index, selected, out),
        }
    }
}

/// `tess_table_format_version`.
#[unsafe(no_mangle)]
pub extern "C" fn tess_table_format_version() -> u32 {
    FORMAT_VERSION
}

/// `tess_table_layout`: the size or offset for a `TessTableLayoutKind`,
/// or 0.
#[unsafe(no_mangle)]
pub extern "C" fn tess_table_layout(kind: c_uint) -> usize {
    match kind {
        0 => HEADER_SIZE,
        1 => VERSION_OFFSET,
        2 => size_of::<TableKey>(),
        3 => offset_of!(TableKey, prepared),
        4 => size_of::<TableStats>(),
        5 => offset_of!(TableStats, region_len),
        6 => size_of::<TableRecord>(),
        7 => offset_of!(TableRecord, payload),
        8 => size_of::<TableRef>(),
        9 => offset_of!(TableRef, nchunks),
        10 => UNIT_BITS as usize,
        _ => 0,
    }
}

/// The key kinds of a configuration.
///
/// # Safety
///
/// `kinds` must point to `nkeys` values when `nkeys` is positive.
unsafe fn key_kinds(nkeys: c_int, kinds: *const c_uint) -> Result<([KeyKind; MAX_KEYS], usize)> {
    let nkeys = usize::try_from(nkeys).unwrap_or(usize::MAX);
    ensure!(
        (1..=MAX_KEYS).contains(&nkeys),
        "a table has 1 to {MAX_KEYS} keys, not {nkeys}"
    );
    ensure!(!kinds.is_null(), "null key kinds");
    // SAFETY: the caller's contract.
    let codes = unsafe { slice::from_raw_parts(kinds, nkeys) };
    let mut result = [KeyKind::Int32; MAX_KEYS];
    for (slot, &code) in result.iter_mut().zip(codes) {
        *slot = match code {
            1 => KeyKind::Int32,
            2 => KeyKind::Int64,
            _ => bail!("unknown key kind {code}"),
        };
    }
    Ok((result, nkeys))
}

/// Decode the keys of a batch into `result`, which starts empty.
///
/// # Safety
///
/// `keys` must point to `nkeys` `TessTableKey`s whose columns satisfy
/// [`DatumColumn::ints`]'s contract with their `prepared` masks, all valid
/// and unchanged for `'a`.
pub(super) unsafe fn table_keys<'a>(
    nkeys: c_int,
    keys: *const TableKey,
    result: &mut TableKeys<'a>,
) -> Result<()> {
    let nkeys = usize::try_from(nkeys).unwrap_or(usize::MAX);
    ensure!(
        (1..=MAX_KEYS).contains(&nkeys),
        "a batch has 1 to {MAX_KEYS} keys, not {nkeys}"
    );
    ensure!(!keys.is_null(), "null keys");
    // SAFETY: the caller's contract, for `'a`.
    let keys = unsafe { slice::from_raw_parts(keys, nkeys) };
    let mut nrows = 0;
    for (index, key) in keys.iter().enumerate() {
        // SAFETY: the caller's contract, for `'a`.
        let (column, prepared) = unsafe {
            (
                key.column.as_ref().context("a null key column")?,
                Mask::view_optional(key.prepared)?,
            )
        };
        // SAFETY: the caller's contract, for `'a`.
        let column = unsafe {
            match key.kind {
                1 => KeyColumn::Int32(column.int32(prepared)?),
                2 => KeyColumn::Int64(column.int64(prepared)?),
                code => bail!("unknown key kind {code}"),
            }
        };
        let column_rows = match &column {
            KeyColumn::Int32(column) => column.nrows(),
            KeyColumn::Int64(column) => column.nrows(),
        };
        ensure!(
            index == 0 || column_rows == nrows,
            "key {index} has {column_rows} rows, the first key {nrows}"
        );
        nrows = column_rows;
        result.columns[index].write(column);
        result.nkeys = index + 1;
    }
    result.nrows = nrows;
    Ok(())
}

/// `nrows` values at `pointer`, empty when there are none.
///
/// # Safety
///
/// `pointer` must point to `nrows` initialized values, valid and unchanged
/// for `'a`, when `nrows` is positive.
pub(super) unsafe fn values<'a, T>(pointer: *const T, nrows: usize, what: &str) -> Result<&'a [T]> {
    if nrows == 0 {
        return Ok(&[]);
    }
    ensure!(!pointer.is_null(), "null {what}");
    // SAFETY: the caller's contract, for `'a`.
    Ok(unsafe { slice::from_raw_parts(pointer, nrows) })
}

/// `nrows` slots at `pointer`, empty when there are none.
///
/// # Safety
///
/// `pointer` must point to `nrows` initialized, writable values that
/// nothing else accesses for `'a`, when `nrows` is positive.
pub(super) unsafe fn slots<'a, T>(
    pointer: *mut T,
    nrows: usize,
    what: &str,
) -> Result<&'a mut [T]> {
    if nrows == 0 {
        return Ok(&mut []);
    }
    ensure!(!pointer.is_null(), "null {what}");
    // SAFETY: the caller's contract, for `'a`.
    Ok(unsafe { slice::from_raw_parts_mut(pointer, nrows) })
}

/// `tess_table_size`: the bytes a table's index needs.
///
/// # Safety
///
/// `kinds` as for [`key_kinds`]; `size` must be null or writable; `status`
/// as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_size(
    nkeys: c_int,
    kinds: *const c_uint,
    payload_size: usize,
    capacity: u64,
    size: *mut usize,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let (kinds, nkeys) = key_kinds(nkeys, kinds)?;
            let config = TableConfig {
                keys: &kinds[..nkeys],
                payload_size,
            };
            let bytes = index_size(&config, capacity)?;
            *size.as_mut().context("a null size")? = bytes;
            Ok(())
        })
    }
}

/// `tess_table_create`: lay an empty table's index out over the region.
///
/// # Safety
///
/// `region` must be aligned to 8 and valid for reads and writes of `len`
/// bytes, accessed by nothing else during the call; `kinds` as for
/// [`key_kinds`]; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_create(
    region: *mut u8,
    len: usize,
    nkeys: c_int,
    kinds: *const c_uint,
    payload_size: usize,
    capacity: u64,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let (kinds, nkeys) = key_kinds(nkeys, kinds)?;
            let config = TableConfig {
                keys: &kinds[..nkeys],
                payload_size,
            };
            TableMut::create(region, len, &config, capacity, Chunks::none()).map(drop)
        })
    }
}

/// `tess_table_stats`: the counts of the table.
///
/// # Safety
///
/// `table` as for [`attach`] during the call; `stats` must be
/// null or writable for its `struct_size`; `status` as for every entry
/// point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_stats(
    table: *const TableRef,
    stats: *mut TableStats,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let table = attach(table)?;
            let out = stats.as_mut().context("a null stats structure")?;
            ensure!(
                out.struct_size >= TableStats::MIN_SIZE,
                "a stats structure is smaller than its required fields"
            );
            let stats = table.stats();
            out.records = stats.records;
            out.buckets = stats.buckets;
            out.bytes_used = stats.bytes_used;
            out.region_len = stats.region_len;
            Ok(())
        })
    }
}

/// `tess_table_chunk_init`: make a block an empty chunk.
///
/// # Safety
///
/// `base` must be aligned to 8 and valid for writes of `len` bytes that
/// nothing else uses yet; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_chunk_init(
    base: *mut u8,
    len: usize,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe { guard(status, || init_chunk(base, len)) }
}

/// `tess_table_append`: append the rows of `pending` to chunk `chunk` of
/// the table's chunks, as long as whole records fit. The table's index is
/// not read: a shared build appends before it has one.
///
/// # Safety
///
/// `table` as for [`chunks_of`], the caller being chunk `chunk`'s one
/// writer; `keys` as for [`table_keys`], their kinds the table's; `pending`
/// must point to a valid mask that nothing else accesses; `hashes` must
/// hold a hash per row, `offsets` a writable slot per row and `payload`
/// null or `payload_size` bytes per row; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_append(
    table: *const TableRef,
    chunk: c_int,
    payload_size: usize,
    hashes: *const u32,
    nkeys: c_int,
    keys: *const TableKey,
    payload: *const u8,
    pending: *mut Mask,
    offsets: *mut u32,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let (_, chunks) = chunks_of(table)?;
            let chunk = usize::try_from(chunk).context("a negative chunk")?;
            let mut decoded = TableKeys::empty();
            table_keys(nkeys, keys, &mut decoded)?;
            let key_list = slice::from_raw_parts(keys, decoded.nkeys);
            let mut kinds = [KeyKind::Int32; MAX_KEYS];
            for (slot, key) in kinds.iter_mut().zip(key_list) {
                *slot = if key.kind == 2 {
                    KeyKind::Int64
                } else {
                    KeyKind::Int32
                };
            }
            let config = TableConfig {
                keys: &kinds[..decoded.nkeys],
                payload_size,
            };
            let mut pending = pending.as_mut().context("a null pending mask")?.mask()?;
            let nrows = pending.as_view().nrows();
            let hashes = values(hashes, nrows, "hashes")?;
            let offsets = slots(offsets, nrows, "offsets")?;
            let payload = if payload.is_null() {
                None
            } else {
                let bytes = nrows
                    .checked_mul(payload_size)
                    .context("the payload does not fit in memory")?;
                Some(values(payload, bytes, "payload")?)
            };
            append_to(
                &config,
                chunks,
                chunk,
                hashes,
                &decoded,
                payload,
                &mut pending,
                offsets,
            )
            .map(drop)
        })
    }
}

/// `tess_table_append_columns`: as [`tess_table_append`], each row's
/// payload a word of its NULL bits and then a word per column of
/// `columns`, which is the table's whole payload.
///
/// # Safety
///
/// As for [`tess_table_append`]; `columns` must point to `ncolumns`
/// columns, each of the mask's rows, valid and unchanged during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_append_columns(
    table: *const TableRef,
    chunk: c_int,
    hashes: *const u32,
    nkeys: c_int,
    keys: *const TableKey,
    ncolumns: c_int,
    columns: *const DatumColumn,
    pending: *mut Mask,
    offsets: *mut u32,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let (_, chunks) = chunks_of(table)?;
            let chunk = usize::try_from(chunk).context("a negative chunk")?;
            let mut decoded = TableKeys::empty();
            table_keys(nkeys, keys, &mut decoded)?;
            let key_list = slice::from_raw_parts(keys, decoded.nkeys);
            let mut kinds = [KeyKind::Int32; MAX_KEYS];
            for (slot, key) in kinds.iter_mut().zip(key_list) {
                *slot = if key.kind == 2 {
                    KeyKind::Int64
                } else {
                    KeyKind::Int32
                };
            }
            let ncolumns = usize::try_from(ncolumns).context("a negative column count")?;
            ensure!(
                ncolumns <= MAX_PAYLOAD_COLUMNS,
                "a payload has up to {MAX_PAYLOAD_COLUMNS} columns, not {ncolumns}"
            );
            let config = TableConfig {
                keys: &kinds[..decoded.nkeys],
                payload_size: 8 * (payload_null_words(ncolumns) + ncolumns),
            };
            let mut pending = pending.as_mut().context("a null pending mask")?.mask()?;
            let nrows = pending.as_view().nrows();
            let hashes = values(hashes, nrows, "hashes")?;
            let offsets = slots(offsets, nrows, "offsets")?;
            let columns = values(columns, ncolumns, "payload columns")?;
            // Only the columns given are set: a batch of a few rows would
            // otherwise pay for clearing 64 slices of each kind. Wider
            // payloads take their slices from the heap.
            let mut words = [const { MaybeUninit::<&[u64]>::uninit() }; 64];
            let mut nulls = [const { MaybeUninit::<&[bool]>::uninit() }; 64];
            let mut wide_words: Vec<&[u64]> = Vec::new();
            let mut wide_nulls: Vec<&[bool]> = Vec::new();
            for (index, column) in columns.iter().enumerate() {
                ensure!(
                    usize::try_from(column.nrows).ok() == Some(nrows),
                    "payload column {index} has {} rows, not {nrows}",
                    column.nrows
                );
                let (value_slice, null_slice) = (
                    values(column.values, nrows, "payload values")?,
                    values(column.isnull, nrows, "payload NULL flags")?,
                );
                if ncolumns <= 64 {
                    words[index].write(value_slice);
                    nulls[index].write(null_slice);
                } else {
                    wide_words.push(value_slice);
                    wide_nulls.push(null_slice);
                }
            }
            // SAFETY: for 64 columns or fewer the loop above initialized
            // the first `ncolumns` slots of each array.
            let (words, nulls) = if ncolumns <= 64 {
                (
                    &*(&raw const words[..ncolumns] as *const [&[u64]]),
                    &*(&raw const nulls[..ncolumns] as *const [&[bool]]),
                )
            } else {
                (wide_words.as_slice(), wide_nulls.as_slice())
            };
            let payload = PayloadColumns::new(words, nulls, nrows)?;
            append_columns_to(
                &config,
                chunks,
                chunk,
                hashes,
                &decoded,
                &payload,
                &mut pending,
                offsets,
            )
            .map(drop)
        })
    }
}

/// The partitions of a spilling table: `npartitions` chunk numbers at
/// `chunks`, and the shift of the partition bits.
///
/// # Safety
///
/// `chunks` must point to `npartitions` numbers, or be null for none.
unsafe fn partitions<'a>(
    chunks: *const u32,
    npartitions: c_int,
    shift: u32,
) -> Result<Partitions<'a>> {
    let count = usize::try_from(npartitions).context("a negative partition count")?;
    // SAFETY: the caller's contract.
    let chunks = unsafe { values(chunks, count, "partition chunks") }?;
    Ok(Partitions { shift, chunks })
}

/// `tess_table_append_partitioned_columns`: append the rows of `pending`,
/// each to the chunk of its hash's partition, as long as whole records fit
/// there, each row's payload taken from `columns` as
/// [`tess_table_append_columns`] takes it; every row appended counts in
/// `rows` at its partition, and its NULL bits go into `*nulls`.
///
/// # Safety
///
/// As for [`tess_table_append_columns`], the caller being the one writer
/// of every partition's chunk; `partition_chunks` as for [`partitions`];
/// `rows` must point to `npartitions`
/// counts and `nulls` to a word, both writable and not accessed by
/// anything else during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_append_partitioned_columns(
    table: *const TableRef,
    partition_chunks: *const u32,
    npartitions: c_int,
    shift: u32,
    hashes: *const u32,
    nkeys: c_int,
    keys: *const TableKey,
    ncolumns: c_int,
    columns: *const DatumColumn,
    pending: *mut Mask,
    offsets: *mut u32,
    rows: *mut u64,
    nulls: *mut u64,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let (_, chunks) = chunks_of(table)?;
            let partitions = partitions(partition_chunks, npartitions, shift)?;
            let mut decoded = TableKeys::empty();
            table_keys(nkeys, keys, &mut decoded)?;
            let key_list = slice::from_raw_parts(keys, decoded.nkeys);
            let mut kinds = [KeyKind::Int32; MAX_KEYS];
            for (slot, key) in kinds.iter_mut().zip(key_list) {
                *slot = if key.kind == 2 {
                    KeyKind::Int64
                } else {
                    KeyKind::Int32
                };
            }
            let ncolumns = usize::try_from(ncolumns).context("a negative column count")?;
            ensure!(
                ncolumns <= 64,
                "a payload has up to 64 columns, not {ncolumns}"
            );
            let config = TableConfig {
                keys: &kinds[..decoded.nkeys],
                payload_size: 8 * (1 + ncolumns),
            };
            let mut pending = pending.as_mut().context("a null pending mask")?.mask()?;
            let nrows = pending.as_view().nrows();
            let hashes = values(hashes, nrows, "hashes")?;
            let offsets = slots(offsets, nrows, "offsets")?;
            let rows = slots(rows, partitions.chunks.len(), "partition row counts")?;
            let nulls = nulls.as_mut().context("a null word of NULL bits")?;
            let columns = values(columns, ncolumns, "payload columns")?;
            // Only the columns given are set, as tess_table_append_columns does.
            let mut words = [const { MaybeUninit::<&[u64]>::uninit() }; 64];
            let mut flags = [const { MaybeUninit::<&[bool]>::uninit() }; 64];
            for (index, column) in columns.iter().enumerate() {
                ensure!(
                    usize::try_from(column.nrows).ok() == Some(nrows),
                    "payload column {index} has {} rows, not {nrows}",
                    column.nrows
                );
                words[index].write(values(column.values, nrows, "payload values")?);
                flags[index].write(values(column.isnull, nrows, "payload NULL flags")?);
            }
            // SAFETY: the loop above initialized the first `ncolumns`
            // slots of each array, and `ncolumns` is at most 64.
            let (words, flags) = (
                &*(&raw const words[..ncolumns] as *const [&[u64]]),
                &*(&raw const flags[..ncolumns] as *const [&[bool]]),
            );
            let payload = PayloadColumns::new(words, flags, nrows)?;
            append_partitioned_columns_to(
                &config,
                chunks,
                &partitions,
                hashes,
                &decoded,
                &payload,
                &mut pending,
                offsets,
                rows,
                nulls,
            )
            .map(drop)
        })
    }
}

/// `tess_table_split`: copy the records of chunk `source` from byte
/// `*from` on, each to the chunk of its hash's partition.
///
/// # Safety
///
/// `table` as for [`chunks_of`], the caller being the one writer of every
/// partition's chunk; `kinds` as for [`key_kinds`]; `partition_chunks` as
/// for [`partitions`]; `from`, `count` and `full` must be writable,
/// `offsets` and `hashes` hold `capacity` writable slots; `status`
/// as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_split(
    table: *const TableRef,
    nkeys: c_int,
    kinds: *const c_uint,
    payload_size: usize,
    partition_chunks: *const u32,
    npartitions: c_int,
    shift: u32,
    source: c_int,
    from: *mut usize,
    capacity: c_int,
    offsets: *mut u32,
    hashes: *mut u32,
    count: *mut c_int,
    full: *mut c_int,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let (_, chunks) = chunks_of(table)?;
            let (kinds, nkeys) = key_kinds(nkeys, kinds)?;
            let config = TableConfig {
                keys: &kinds[..nkeys],
                payload_size,
            };
            let partitions = partitions(partition_chunks, npartitions, shift)?;
            let source = usize::try_from(source).context("a negative chunk")?;
            let capacity = usize::try_from(capacity).context("a negative capacity")?;
            let offsets = slots(offsets, capacity, "offsets")?;
            let hashes = slots(hashes, capacity, "hashes")?;
            let from = from.as_mut().context("a null from")?;
            let split = split_to(&config, chunks, &partitions, source, from, offsets, hashes)?;
            *count.as_mut().context("a null count")? = split.count as c_int;
            *full.as_mut().context("a null full")? =
                split.full.map_or(-1, |partition| partition as c_int);
            Ok(())
        })
    }
}

/// `tess_table_link`: link a chunk's records from byte `*from` into the
/// buckets; several participants may link chunks of their own at once.
/// With `duplicates`, also count the records whose keys the table held
/// already.
///
/// # Safety
///
/// `table` as for [`attach`] during the call, the caller linking chunk
/// `chunk` alone; `from` must point to a writable size, `linked` and
/// `duplicates` be null or writable; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_link(
    table: *const TableRef,
    chunk: c_int,
    from: *mut usize,
    linked: *mut u64,
    duplicates: *mut u64,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let chunk = usize::try_from(chunk).context("a negative chunk")?;
            let from = from.as_mut().context("a null link cursor")?;
            let table = attach(table)?;
            let count = match duplicates.as_mut() {
                Some(duplicates) => {
                    let (count, repeated) = table.link_counting(chunk, from)?;
                    *duplicates = repeated as u64;
                    count
                }
                None => table.link(chunk, from)?,
            };
            if let Some(linked) = linked.as_mut() {
                *linked = count as u64;
            }
            Ok(())
        })
    }
}

/// `tess_table_link_grouped`: link a chunk's records next to those of the
/// same keys, as the table's one writer.
///
/// # Safety
///
/// `table` as for [`attach_mut`] during the call; `from` must point to a
/// writable size, `linked` and `duplicates` be null or writable; `status` as
/// for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_link_grouped(
    table: *const TableRef,
    chunk: c_int,
    from: *mut usize,
    linked: *mut u64,
    duplicates: *mut u64,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let chunk = usize::try_from(chunk).context("a negative chunk")?;
            let from = from.as_mut().context("a null link cursor")?;
            let (count, repeated) = attach_mut(table)?.link_grouped(chunk, from)?;
            if let Some(linked) = linked.as_mut() {
                *linked = count as u64;
            }
            if let Some(duplicates) = duplicates.as_mut() {
                *duplicates = repeated as u64;
            }
            Ok(())
        })
    }
}

/// `tess_table_probe`: find the newest record of each row's keys.
///
/// # Safety
///
/// `table` as for [`attach`] during the call; `keys` as for
/// [`table_keys`]; `rows` must point to a valid mask and `found` to a
/// valid mask that nothing else accesses, both with the batch's rows;
/// `hashes` must hold a hash per row and `matches` a writable slot per
/// row; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_probe(
    table: *const TableRef,
    hashes: *const u32,
    nkeys: c_int,
    keys: *const TableKey,
    rows: *const Mask,
    matches: *mut u32,
    found: *mut Mask,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let table = attach(table)?;
            let mut decoded = TableKeys::empty();
            table_keys(nkeys, keys, &mut decoded)?;
            let rows = rows.as_ref().context("a null row mask")?.view()?;
            let mut found = found.as_mut().context("a null result mask")?.mask()?;
            let nrows = rows.nrows();
            let hashes = values(hashes, nrows, "hashes")?;
            let matches = slots(matches, nrows, "matches")?;
            table.probe(hashes, &decoded, &rows, matches, &mut found)
        })
    }
}

/// `tess_table_next_match`: replace each row's record offset by the next
/// record with the same keys.
///
/// # Safety
///
/// `table` as for [`attach`] during the call; `rows` must point
/// to a valid mask and `found` to a valid mask that nothing else accesses,
/// both with the batch's rows; `offsets` must hold an initialized,
/// writable offset per row that nothing else accesses; `status` as for
/// every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_next_match(
    table: *const TableRef,
    offsets: *mut u32,
    rows: *const Mask,
    found: *mut Mask,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let table = attach(table)?;
            let rows = rows.as_ref().context("a null row mask")?.view()?;
            let mut found = found.as_mut().context("a null result mask")?.mask()?;
            let offsets = slots(offsets, rows.nrows(), "offsets")?;
            table.next_match(offsets, &rows, &mut found)
        })
    }
}

/// `tess_table_gather`: one payload word of each selected row's record.
///
/// # Safety
///
/// `table` as for [`attach`] during the call; `rows` must point
/// to a valid mask; `offsets` must hold an initialized offset per row and
/// `out` an initialized, writable word per row that nothing else
/// accesses; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_gather(
    table: *const TableRef,
    offsets: *const u32,
    rows: *const Mask,
    at: usize,
    out: *mut u64,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let table = attach(table)?;
            let rows = rows.as_ref().context("a null row mask")?.view()?;
            let nrows = rows.nrows();
            let offsets = values(offsets, nrows, "offsets")?;
            let out = slots(out, nrows, "results")?;
            table.gather(offsets, &rows, at, out)
        })
    }
}

/// `tess_table_gather_scattered`: as [`tess_table_gather`], prefetching
/// the records of a word of rows before reading them.
///
/// # Safety
///
/// As for [`tess_table_gather`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_gather_scattered(
    table: *const TableRef,
    offsets: *const u32,
    rows: *const Mask,
    at: usize,
    out: *mut u64,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let table = attach(table)?;
            let rows = rows.as_ref().context("a null row mask")?.view()?;
            let nrows = rows.nrows();
            let offsets = values(offsets, nrows, "offsets")?;
            let out = slots(out, nrows, "results")?;
            table.gather_scattered(offsets, &rows, at, out)
        })
    }
}

/// `tess_table_next_in_group`: step each row to the record right after
/// its own when that one has the same keys.
///
/// # Safety
///
/// As for [`tess_table_next_match`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_next_in_group(
    table: *const TableRef,
    offsets: *mut u32,
    rows: *const Mask,
    found: *mut Mask,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let table = attach(table)?;
            let rows = rows.as_ref().context("a null row mask")?.view()?;
            let mut found = found.as_mut().context("a null result mask")?.mask()?;
            let offsets = slots(offsets, rows.nrows(), "offsets")?;
            table.next_in_group(offsets, &rows, &mut found)
        })
    }
}

/// `tess_table_record`: the record at an offset.
///
/// # Safety
///
/// `table` as for [`attach`] during the call and until the caller
/// is done with the pointers it receives; `record` must be null or
/// writable for its `struct_size`; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_record(
    table: *const TableRef,
    offset: u32,
    record: *mut TableRecord,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let table = attach(table)?;
            let out = record.as_mut().context("a null record structure")?;
            ensure!(
                out.struct_size >= TableRecord::MIN_SIZE,
                "a record structure is smaller than its required fields"
            );
            let record = table.record(offset)?;
            out.hash = record.hash;
            out.null_bits = record.null_bits;
            out.keys = record.keys.as_ptr();
            out.payload = record.payload.as_ptr();
            out.payload_size = record.payload.len();
            Ok(())
        })
    }
}

/// `tess_table_find_or_insert`: give each pending row the record of its
/// keys, creating one where none exists.
///
/// # Safety
///
/// `table` as for [`attach_mut`] during the call; `keys` as
/// for [`table_keys`]; `pending` and `inserted` must point to valid masks
/// that nothing else accesses, with the batch's rows; `hashes` must hold
/// a hash per row and `offsets` a writable slot per row; `status` as for
/// every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_find_or_insert(
    table: *const TableRef,
    chunk: c_int,
    hashes: *const u32,
    nkeys: c_int,
    keys: *const TableKey,
    pending: *mut Mask,
    offsets: *mut u32,
    inserted: *mut Mask,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let mut table = attach_mut(table)?;
            let chunk = usize::try_from(chunk).context("a negative chunk")?;
            let mut decoded = TableKeys::empty();
            table_keys(nkeys, keys, &mut decoded)?;
            let mut pending = pending.as_mut().context("a null pending mask")?.mask()?;
            let mut inserted = inserted.as_mut().context("a null inserted mask")?.mask()?;
            let nrows = pending.as_view().nrows();
            let hashes = values(hashes, nrows, "hashes")?;
            let offsets = slots(offsets, nrows, "offsets")?;
            table
                .find_or_insert(
                    chunk,
                    hashes,
                    &decoded,
                    &mut pending,
                    offsets,
                    &mut inserted,
                )
                .map(drop)
        })
    }
}

/// `tess_table_find_or_insert_partitioned`: resolve each pending row to
/// the record of its keys, a new one going to its partition's chunk.
///
/// # Safety
///
/// As for [`tess_table_find_or_insert`], the caller being the one writer
/// of every partition's chunk; `partition_chunks` as for [`partitions`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_find_or_insert_partitioned(
    table: *const TableRef,
    partition_chunks: *const u32,
    npartitions: c_int,
    shift: u32,
    hashes: *const u32,
    nkeys: c_int,
    keys: *const TableKey,
    pending: *mut Mask,
    offsets: *mut u32,
    inserted: *mut Mask,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let mut table = attach_mut(table)?;
            let partitions = partitions(partition_chunks, npartitions, shift)?;
            let mut decoded = TableKeys::empty();
            table_keys(nkeys, keys, &mut decoded)?;
            let mut pending = pending.as_mut().context("a null pending mask")?.mask()?;
            let mut inserted = inserted.as_mut().context("a null inserted mask")?.mask()?;
            let nrows = pending.as_view().nrows();
            let hashes = values(hashes, nrows, "hashes")?;
            let offsets = slots(offsets, nrows, "offsets")?;
            table
                .find_or_insert_partitioned(
                    &partitions,
                    hashes,
                    &decoded,
                    &mut pending,
                    offsets,
                    &mut inserted,
                )
                .map(drop)
        })
    }
}

/// `tess_table_combine`: merge the groups' states of a chunk into the
/// table.
///
/// # Safety
///
/// `table` as for [`attach_mut`] during the call; `combines` must point to
/// `naggregates` codes; `from`, `merged` and `stop` must be writable;
/// `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_combine(
    table: *const TableRef,
    source: c_int,
    from: *mut usize,
    chunk: c_int,
    naggregates: c_int,
    combines: *const c_uint,
    merged: *mut c_int,
    stop: *mut c_int,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let mut table = attach_mut(table)?;
            let source = usize::try_from(source).context("a negative chunk")?;
            let chunk = usize::try_from(chunk).context("a negative chunk")?;
            let count = usize::try_from(naggregates).context("a negative aggregate count")?;
            ensure!(
                count <= 64,
                "{count} aggregates are more than a word of flags holds"
            );
            let codes = values(combines, count, "combines")?;
            let mut decoded = [Combine::Count; 64];
            for (slot, &code) in decoded.iter_mut().zip(codes) {
                *slot = match code {
                    1 => Combine::Count,
                    2 => Combine::Sum,
                    3 => Combine::Min,
                    4 => Combine::Max,
                    _ => bail!("unknown combine {code}"),
                };
            }
            let from = from.as_mut().context("a null from")?;
            let (count_merged, stopped) = table.combine(source, from, chunk, &decoded[..count])?;
            *merged.as_mut().context("a null merged count")? = count_merged as c_int;
            *stop.as_mut().context("a null stop")? = match stopped {
                CombineStop::Done => 0,
                CombineStop::ChunkFull => 1,
                CombineStop::IndexFull => 2,
            };
            Ok(())
        })
    }
}

/// `tess_table_payloads`: the payload of the record of each selected
/// row, to change in place, in one call for a batch.
///
/// # Safety
///
/// `table` as for [`attach_mut`] during the call and until the caller
/// is done with the pointers it receives; `rows` must point to a valid
/// mask; `offsets` must hold an initialized offset per row and
/// `payloads` a writable pointer per row; `status` as for every entry
/// point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_payloads(
    table: *const TableRef,
    offsets: *const u32,
    rows: *const Mask,
    payloads: *mut *mut u8,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let mut table = attach_mut(table)?;
            let rows = rows.as_ref().context("a null row mask")?.view()?;
            let nrows = rows.nrows();
            let offsets = values(offsets, nrows, "offsets")?;
            let payloads = slots(payloads, nrows, "payload pointers")?;
            for row in rows.selected_indices() {
                payloads[row] = table.payload_mut(offsets[row])?.as_mut_ptr();
            }
            Ok(())
        })
    }
}

/// `tess_table_scan`: the next records in insertion order.
///
/// # Safety
///
/// `table` as for [`attach_mut`] during the call; `cursor` and
/// `count` must be null or writable; `offsets` must hold `capacity`
/// writable slots; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_scan(
    table: *const TableRef,
    cursor: *mut u64,
    offsets: *mut u32,
    capacity: c_int,
    count: *mut c_int,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let table = attach_mut(table)?;
            let raw = cursor.as_mut().context("a null cursor")?;
            let mut cursor = if *raw == 0 {
                Cursor::start()
            } else {
                Cursor::from_raw(*raw)
            };
            let capacity = usize::try_from(capacity).context("a negative capacity")?;
            let out = slots(offsets, capacity, "offsets")?;
            let visited = table.scan(&mut cursor, out)?;
            *raw = cursor.raw();
            *count.as_mut().context("a null count")? = visited as c_int;
            Ok(())
        })
    }
}

/// `tess_table_regrow`: move the table to a new index of `len` bytes at
/// `index`, for `capacity` records, over the same chunks; the old index is
/// no longer the table's.
///
/// # Safety
///
/// `table` as for [`attach_mut`] during the call; `index` must be aligned to
/// 8 and valid for reads and writes of `len` bytes that nothing else uses;
/// `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_regrow(
    table: *const TableRef,
    index: *mut u8,
    len: usize,
    capacity: u64,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe { guard(status, || attach_mut(table)?.regrow(index, len, capacity)) }
}

/// `TessTableAccumulate`: how `tess_table_accumulate` updates a state.
const ACCUMULATE_COUNT_ROWS: c_uint = 1;
const ACCUMULATE_COUNT: c_uint = 2;
const ACCUMULATE_SUM_INT4: c_uint = 3;
const ACCUMULATE_MIN_INT4: c_uint = 4;
const ACCUMULATE_MAX_INT4: c_uint = 5;
const ACCUMULATE_MIN_INT8: c_uint = 6;
const ACCUMULATE_MAX_INT8: c_uint = 7;
const ACCUMULATE_SUM_INT8: c_uint = 8;

/// `tess_table_accumulate`: fold each selected row into the aggregate
/// state of its record's payload.
///
/// # Safety
///
/// `table` as for [`attach_mut`] during the call; `rows` must
/// point to a valid mask; `offsets` must hold an initialized offset per
/// row; `column` is ignored for `count(*)` and otherwise must satisfy
/// [`DatumColumn::ints`]'s contract with `prepared` as its readiness;
/// `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_accumulate(
    table: *const TableRef,
    offsets: *const u32,
    rows: *const Mask,
    op: c_uint,
    column: *const DatumColumn,
    prepared: *const Mask,
    value_at: usize,
    flags_at: usize,
    flag_bit: c_uint,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let mut table = attach_mut(table)?;
            let rows = rows.as_ref().context("a null row mask")?.view()?;
            let offsets = values(offsets, rows.nrows(), "offsets")?;
            let slot = Slot {
                value_at,
                flags_at,
                flag_bit,
            };
            match op {
                ACCUMULATE_COUNT_ROWS => table.count_rows(offsets, &rows, value_at),
                ACCUMULATE_COUNT => {
                    let column = reader::<()>(column, prepared)?;
                    table.count_values(offsets, &rows, &column, value_at)
                }
                ACCUMULATE_SUM_INT4 | ACCUMULATE_MIN_INT4 | ACCUMULATE_MAX_INT4 => {
                    let column = reader::<i32>(column, prepared)?;
                    let fold = match op {
                        ACCUMULATE_SUM_INT4 => Fold::Sum,
                        ACCUMULATE_MIN_INT4 => Fold::Min,
                        _ => Fold::Max,
                    };
                    table.fold(offsets, &rows, &column, fold, slot)
                }
                ACCUMULATE_MIN_INT8 | ACCUMULATE_MAX_INT8 | ACCUMULATE_SUM_INT8 => {
                    let column = reader::<i64>(column, prepared)?;
                    let fold = match op {
                        ACCUMULATE_MIN_INT8 => Fold::Min,
                        ACCUMULATE_MAX_INT8 => Fold::Max,
                        _ => Fold::Sum,
                    };
                    table.fold(offsets, &rows, &column, fold, slot)
                }
                other => bail!("unknown accumulation {other}"),
            }
        })
    }
}

/// `TessTableSumInput`: the values `tess_table_accumulate_sums` reads.
const SUM_NUMERIC: c_uint = 0;
const SUM_INT4: c_uint = 1;
const SUM_INT8: c_uint = 2;

/// `TessTableSumArg`: a sum of `tess_table_accumulate_sums`.
#[repr(C)]
#[derive(Debug)]
pub struct TableSumArg {
    /// A `TessTableSumInput`.
    pub kind: c_uint,
    /// Its column.
    pub column: *const DatumColumn,
    /// Its state's first byte in the payload.
    pub value_at: usize,
    /// The rows the state does not take.
    pub rest: *mut Mask,
}

/// `tess_table_accumulate_sums`: fold each selected row into the sum or
/// average states of its record's payload, the rows a state does not take
/// into its sum's rest.
///
/// # Safety
///
/// `table` as for [`attach_mut`] during the call; `rows` must point to a
/// valid mask; `offsets` must hold an initialized offset per row; `sums`
/// must point to `nsums` sums, each with a valid `TessDatumColumn` of the
/// rows' count whose selected rows have initialized flags and, when not
/// NULL, values of the kind (numeric Datums read in place, or its
/// decimals, or integer words), and a valid rest mask that nothing else
/// accesses; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_accumulate_sums(
    table: *const TableRef,
    offsets: *const u32,
    rows: *const Mask,
    nsums: c_int,
    sums: *const TableSumArg,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let mut table = attach_mut(table)?;
            let rows = rows.as_ref().context("a null row mask")?.view()?;
            let nrows = rows.nrows();
            let offsets = values(offsets, nrows, "offsets")?;
            let nsums = usize::try_from(nsums).context("a negative sum count")?;
            ensure!(nsums <= MAX_SUMS, "{nsums} sums in a call, past {MAX_SUMS}");
            let args = values(sums, nsums, "sums")?;
            // Only the sums given are set, as in the partitioned appends.
            let mut columns = [const { MaybeUninit::<SumColumn<'_>>::uninit() }; MAX_SUMS];
            for (arg, column) in args.iter().zip(columns.iter_mut()) {
                let input = match arg.kind {
                    SUM_NUMERIC => SumInput::Numeric,
                    SUM_INT4 => SumInput::Int4,
                    SUM_INT8 => SumInput::Int8,
                    other => bail!("unknown sum input {other}"),
                };
                column.write(SumColumn::new(input, arg.column, nrows)?);
            }
            // SAFETY: the loop above initialized the first `nsums` columns.
            let columns = &*(&raw const columns[..nsums] as *const [SumColumn<'_>]);
            let mut slots =
                [const { MaybeUninit::<SumSlot<'_, SumColumn<'_>>>::uninit() }; MAX_SUMS];
            for ((arg, column), slot) in args.iter().zip(columns).zip(slots.iter_mut()) {
                slot.write(SumSlot {
                    terms: column,
                    at: arg.value_at,
                    rest: arg.rest.as_mut().context("a null rest mask")?.mask()?,
                });
            }
            // SAFETY: as above, and the slots own nothing to drop.
            let slots = &mut *(&raw mut slots[..nsums] as *mut [SumSlot<'_, SumColumn<'_>>]);
            table.sum_terms(offsets, &rows, slots)
        })
    }
}

/// `tess_table_gather_key`: one key of each selected row's record, as its
/// Datum and NULL flag.
///
/// # Safety
///
/// `table` as for [`attach`] during the call; `rows` must point
/// to a valid mask; `offsets` must hold an initialized offset per row,
/// `values` a writable Datum and `isnull` a writable flag per row that
/// nothing else accesses; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_gather_key(
    table: *const TableRef,
    offsets: *const u32,
    rows: *const Mask,
    key: c_int,
    values_out: *mut u64,
    isnull: *mut bool,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let table = attach(table)?;
            let rows = rows.as_ref().context("a null row mask")?.view()?;
            let nrows = rows.nrows();
            let offsets = values(offsets, nrows, "offsets")?;
            let out = slots(values_out, nrows, "results")?;
            let nulls = slots(isnull, nrows, "NULL flags")?;
            let key = usize::try_from(key).context("a negative key")?;
            table.gather_key(offsets, &rows, key, out, nulls)
        })
    }
}

/// `tess_table_bloom_words`: the words of a Bloom filter for a table of
/// `records` records.
///
/// # Safety
///
/// `nwords` must point to a writable size; `status` as for every entry
/// point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_bloom_words(
    records: u64,
    nwords: *mut usize,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let out = nwords.as_mut().context("a null result")?;
            *out = tessera_kernels::table::bloom::words_for(records)?;
            Ok(())
        })
    }
}

/// `tess_table_bloom`: fill a Bloom filter with the table's records.
///
/// # Safety
///
/// `table` as for [`attach`] during the call, with no insertion
/// running; `words` must point to `nwords` writable words that nothing
/// else accesses; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_bloom(
    table: *const TableRef,
    words: *mut u64,
    nwords: usize,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let table = attach(table)?;
            let words = slots(words, nwords, "filter words")?;
            table.bloom(words)
        })
    }
}

/// `tess_bloom_add`: set the bits of the hashes of a batch's rows.
///
/// # Safety
///
/// `words` must point to `nwords` writable words that nothing else
/// accesses; `rows` to a valid mask; `hashes` to a hash per row; `status`
/// as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_bloom_add(
    words: *mut u64,
    nwords: usize,
    hashes: *const u32,
    rows: *const Mask,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let rows = rows.as_ref().context("a null row mask")?.view()?;
            let words = slots(words, nwords, "filter words")?;
            let hashes = values(hashes, rows.nrows(), "hashes")?;
            tessera_kernels::table::bloom::add(words, hashes, &rows)
        })
    }
}

/// `tess_bloom_probe`: the rows of a batch whose hash the filter lets
/// through.
///
/// # Safety
///
/// `words` must point to `nwords` initialized words; `rows` and `found`
/// to valid masks of the same row count that do not overlap; `hashes` to
/// a hash per row; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_bloom_probe(
    words: *const u64,
    nwords: usize,
    hashes: *const u32,
    rows: *const Mask,
    found: *mut Mask,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let rows = rows.as_ref().context("a null row mask")?.view()?;
            let mut found = found.as_mut().context("a null result mask")?.mask()?;
            let words = values(words, nwords, "filter words")?;
            let hashes = values(hashes, rows.nrows(), "hashes")?;
            tessera_kernels::table::bloom::probe(words, hashes, &rows, &mut found)
        })
    }
}

/// `tess_bloom_shared_words`: the words of a shared Bloom filter, its
/// state word included, for a table of `records` records.
///
/// # Safety
///
/// `nwords` must point to a writable size; `status` as for every entry
/// point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_bloom_shared_words(
    records: u64,
    nwords: *mut usize,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let out = nwords.as_mut().context("a null result")?;
            *out = tessera_kernels::table::bloom::shared_words_for(records)?;
            Ok(())
        })
    }
}

/// Attach to a shared filter for the length of a call.
///
/// # Safety
///
/// `words` as for [`SharedFilter::attach`] during the call.
unsafe fn shared_filter<'a>(words: *mut u64, nwords: usize) -> Result<SharedFilter<'a>> {
    // SAFETY: the caller's contract.
    unsafe { SharedFilter::attach(words, nwords) }
}

/// `tess_bloom_shared_init`: clear a shared filter before any participant
/// uses it.
///
/// # Safety
///
/// `words` must point to `nwords` words aligned to 8 that only shared
/// filters access and no other participant uses yet; `status` as for
/// every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_bloom_shared_init(
    words: *mut u64,
    nwords: usize,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            shared_filter(words, nwords).map(|filter| filter.init())
        })
    }
}

/// `tess_table_try_build_bloom`: build a shared filter of the table's
/// records unless another participant has claimed it.
///
/// # Safety
///
/// `table` as for [`attach`] during the call, with no insertion
/// running; `words` as for [`tess_bloom_shared_init`], other participants
/// using it too; `built` must point to a writable flag; `status` as for
/// every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_try_build_bloom(
    table: *const TableRef,
    words: *mut u64,
    nwords: usize,
    built: *mut bool,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let table = attach(table)?;
            let filter = shared_filter(words, nwords)?;
            let built = built.as_mut().context("a null result")?;
            *built = table.try_build_bloom(&filter)?;
            Ok(())
        })
    }
}

/// `tess_bloom_shared_ready`: whether a shared filter is built.
///
/// # Safety
///
/// `words` as for [`tess_table_try_build_bloom`]; `ready` must point to a
/// writable flag; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_bloom_shared_ready(
    words: *mut u64,
    nwords: usize,
    ready: *mut bool,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let filter = shared_filter(words, nwords)?;
            *ready.as_mut().context("a null result")? = filter.ready();
            Ok(())
        })
    }
}

/// `tess_bloom_shared_probe`: [`tess_bloom_probe`] against a shared filter,
/// which must be ready.
///
/// # Safety
///
/// `words` as for [`tess_table_try_build_bloom`]; `rows`, `found` and
/// `hashes` as for [`tess_bloom_probe`]; `status` as for every entry
/// point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_bloom_shared_probe(
    words: *mut u64,
    nwords: usize,
    hashes: *const u32,
    rows: *const Mask,
    found: *mut Mask,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let filter = shared_filter(words, nwords)?;
            let rows = rows.as_ref().context("a null row mask")?.view()?;
            let mut found = found.as_mut().context("a null result mask")?.mask()?;
            let hashes = values(hashes, rows.nrows(), "hashes")?;
            tessera_kernels::table::bloom::probe_shared(&filter, hashes, &rows, &mut found)
        })
    }
}

/// Attach to a build's counters for the length of a call.
///
/// # Safety
///
/// `counters` as for [`SharedCounters::attach`] during the call.
unsafe fn build_counters<'a>(counters: *mut u64) -> Result<SharedCounters<'a>> {
    // SAFETY: the caller's contract.
    unsafe { SharedCounters::attach(counters) }
}

/// `tess_build_counters_init`: clear a shared build's counters before any
/// participant attaches.
///
/// # Safety
///
/// `counters` must point to `TESS_BUILD_COUNTER_WORDS` words aligned to 8
/// that only build counters access; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_build_counters_init(counters: *mut u64, status: *mut Status) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            build_counters(counters).map(|counters| counters.init())
        })
    }
}

/// `tess_build_report`: add the records a participant appended and the
/// payload words it saw a NULL in.
///
/// # Safety
///
/// As [`tess_build_counters_init`], other participants using them too.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_build_report(
    counters: *mut u64,
    records: u64,
    null_columns: u64,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            build_counters(counters).map(|counters| counters.report(records, null_columns))
        })
    }
}

/// `tess_build_take_chunk`: the number of a new chunk of a shared build.
///
/// # Safety
///
/// As [`tess_build_report`]; `number` must point to a writable word.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_build_take_chunk(
    counters: *mut u64,
    number: *mut u64,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let counters = build_counters(counters)?;
            *number.as_mut().context("a null result")? = counters.take_chunk();
            Ok(())
        })
    }
}

/// `tess_build_add_duplicates`: add the duplicates a participant's links
/// found, before it arrives at the barrier after linking.
///
/// # Safety
///
/// As [`tess_build_report`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_build_add_duplicates(
    counters: *mut u64,
    duplicates: u64,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            build_counters(counters)?.add_duplicates(duplicates);
            Ok(())
        })
    }
}

/// `tess_build_totals`: the records every participant appended, the
/// payload words with a NULL, the chunks numbered and, once linking is
/// over, the duplicates the links found.
///
/// # Safety
///
/// As [`tess_build_report`]; `records`, `null_columns` and `chunks` must
/// point to writable words, `duplicates` be null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_build_totals(
    counters: *mut u64,
    records: *mut u64,
    null_columns: *mut u64,
    chunks: *mut u64,
    duplicates: *mut u64,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let counters = build_counters(counters)?;
            *records.as_mut().context("a null result")? = counters.total_records();
            *null_columns.as_mut().context("a null result")? = counters.nulls();
            *chunks.as_mut().context("a null result")? = counters.total_chunks();
            if let Some(duplicates) = duplicates.as_mut() {
                *duplicates = counters.total_duplicates();
            }
            Ok(())
        })
    }
}

/// `tess_build_step`: a participant's next action, after the previous one
/// is done, given what its barrier operation returned.
///
/// # Safety
///
/// `participant` must point to a participant this process alone uses,
/// zeroed before its first step; `counters` as for [`tess_build_report`];
/// `action` must point to a writable code; `status` as for every entry
/// point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_build_step(
    participant: *mut Participant,
    counters: *mut u64,
    reply: u32,
    action: *mut u32,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let participant = participant.as_mut().context("a null participant")?;
            let counters = build_counters(counters)?;
            let next = participant.step(&counters, reply)?;
            *action.as_mut().context("a null action")? = next as u32;
            Ok(())
        })
    }
}

/// `tess_table_fingerprint`: the fingerprint of the table's record layout.
///
/// # Safety
///
/// `table` as for [`attach`] during the call; `fingerprint` must point to
/// a writable word; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_fingerprint(
    table: *const TableRef,
    fingerprint: *mut u64,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            *fingerprint.as_mut().context("a null fingerprint")? = attach(table)?.fingerprint();
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{TableKey, TableRecord, TableStats};

    #[test]
    fn a_participant_is_three_words_of_four_bytes() {
        assert_eq!(size_of::<tessera_kernels::table::phases::Participant>(), 12);
        assert_eq!(align_of::<tessera_kernels::table::phases::Participant>(), 4);
    }

    #[test]
    fn layouts_match_the_header() {
        assert_eq!(size_of::<TableKey>(), 24);
        assert_eq!(std::mem::offset_of!(TableKey, prepared), 16);
        assert_eq!(size_of::<TableStats>(), 40);
        assert_eq!(TableStats::MIN_SIZE, 40);
        assert_eq!(size_of::<TableRecord>(), 40);
        assert_eq!(TableRecord::MIN_SIZE, 40);
        assert_eq!(std::mem::offset_of!(TableRecord, payload), 24);
    }
}
