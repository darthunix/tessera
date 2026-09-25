//! The table entry points, declared in `include/tessera/table.h`.
//!
//! The region comes as a pointer and a length with every call, and every
//! call attaches anew: the library keeps nothing between calls. Keys come
//! as `TessTableKey`s, Datum columns read by their kind, and reach the
//! table through [`TableKeys`].

use std::ffi::{c_int, c_uint};
use std::mem::{MaybeUninit, offset_of};
use std::slice;

use anyhow::{Context, Result, bail, ensure};
use tessera_core::ColumnReader;
use tessera_kernels::table::{
    Cursor, FORMAT_VERSION, Fold, HEADER_SIZE, KeyKind, KeySource, MAX_KEYS, Slot, Table,
    TableConfig, TableMut, VERSION_OFFSET, bloom::SharedFilter, normalize_word, region_size,
};

use super::args::reader;
use super::column::DatumColumn;
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
    fn empty() -> Self {
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
unsafe fn table_keys<'a>(
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
unsafe fn values<'a, T>(pointer: *const T, nrows: usize, what: &str) -> Result<&'a [T]> {
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
unsafe fn slots<'a, T>(pointer: *mut T, nrows: usize, what: &str) -> Result<&'a mut [T]> {
    if nrows == 0 {
        return Ok(&mut []);
    }
    ensure!(!pointer.is_null(), "null {what}");
    // SAFETY: the caller's contract, for `'a`.
    Ok(unsafe { slice::from_raw_parts_mut(pointer, nrows) })
}

/// `tess_table_size`: the bytes a region needs for a table.
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
            let bytes = region_size(&config, capacity)?;
            *size.as_mut().context("a null size")? = bytes;
            Ok(())
        })
    }
}

/// `tess_table_create`: lay an empty table out over the region.
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
            TableMut::create(region, len, &config, capacity).map(drop)
        })
    }
}

/// `tess_table_attach`: check that the region holds a table.
///
/// # Safety
///
/// `region` as for [`Table::attach`] during the call; `status` as for
/// every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_attach(
    region: *const u8,
    len: usize,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe { guard(status, || Table::attach(region.cast_mut(), len).map(drop)) }
}

/// `tess_table_stats`: the counts of the table.
///
/// # Safety
///
/// `region` as for [`Table::attach`] during the call; `stats` must be
/// null or writable for its `struct_size`; `status` as for every entry
/// point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_stats(
    region: *const u8,
    len: usize,
    stats: *mut TableStats,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let table = Table::attach(region.cast_mut(), len)?;
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

/// `tess_table_insert`: insert the rows of `pending` until the table has
/// no room.
///
/// # Safety
///
/// `region` as for [`Table::attach`] during the call; `keys` as for
/// [`table_keys`]; `pending` must point to a valid mask that nothing else
/// accesses; `hashes` must hold a hash per row, `offsets` a writable slot
/// per row and `payload` null or the table's payload size times the rows;
/// `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_insert(
    region: *mut u8,
    len: usize,
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
            let table = Table::attach(region, len)?;
            let mut decoded = TableKeys::empty();
            table_keys(nkeys, keys, &mut decoded)?;
            let mut pending = pending.as_mut().context("a null pending mask")?.mask()?;
            let nrows = pending.as_view().nrows();
            let hashes = values(hashes, nrows, "hashes")?;
            let offsets = slots(offsets, nrows, "offsets")?;
            let payload = if payload.is_null() {
                None
            } else {
                let bytes = nrows
                    .checked_mul(table.payload_size())
                    .context("the payload does not fit in memory")?;
                Some(values(payload, bytes, "payload")?)
            };
            table
                .insert(hashes, &decoded, payload, &mut pending, offsets)
                .map(drop)
        })
    }
}

/// `tess_table_probe`: find the newest record of each row's keys.
///
/// # Safety
///
/// `region` as for [`Table::attach`] during the call; `keys` as for
/// [`table_keys`]; `rows` must point to a valid mask and `found` to a
/// valid mask that nothing else accesses, both with the batch's rows;
/// `hashes` must hold a hash per row and `matches` a writable slot per
/// row; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_probe(
    region: *const u8,
    len: usize,
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
            let table = Table::attach(region.cast_mut(), len)?;
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
/// `region` as for [`Table::attach`] during the call; `rows` must point
/// to a valid mask and `found` to a valid mask that nothing else accesses,
/// both with the batch's rows; `offsets` must hold an initialized,
/// writable offset per row that nothing else accesses; `status` as for
/// every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_next_match(
    region: *const u8,
    len: usize,
    offsets: *mut u32,
    rows: *const Mask,
    found: *mut Mask,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let table = Table::attach(region.cast_mut(), len)?;
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
/// `region` as for [`Table::attach`] during the call; `rows` must point
/// to a valid mask; `offsets` must hold an initialized offset per row and
/// `out` an initialized, writable word per row that nothing else
/// accesses; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_gather(
    region: *const u8,
    len: usize,
    offsets: *const u32,
    rows: *const Mask,
    at: usize,
    out: *mut u64,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let table = Table::attach(region.cast_mut(), len)?;
            let rows = rows.as_ref().context("a null row mask")?.view()?;
            let nrows = rows.nrows();
            let offsets = values(offsets, nrows, "offsets")?;
            let out = slots(out, nrows, "results")?;
            table.gather(offsets, &rows, at, out)
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
    region: *const u8,
    len: usize,
    offsets: *mut u32,
    rows: *const Mask,
    found: *mut Mask,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let table = Table::attach(region.cast_mut(), len)?;
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
/// `region` as for [`Table::attach`] during the call and until the caller
/// is done with the pointers it receives; `record` must be null or
/// writable for its `struct_size`; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_record(
    region: *const u8,
    len: usize,
    offset: u32,
    record: *mut TableRecord,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let table = Table::attach(region.cast_mut(), len)?;
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
/// `region` as for [`TableMut::attach_mut`] during the call; `keys` as
/// for [`table_keys`]; `pending` and `inserted` must point to valid masks
/// that nothing else accesses, with the batch's rows; `hashes` must hold
/// a hash per row and `offsets` a writable slot per row; `status` as for
/// every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_find_or_insert(
    region: *mut u8,
    len: usize,
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
            let mut table = TableMut::attach_mut(region, len)?;
            let mut decoded = TableKeys::empty();
            table_keys(nkeys, keys, &mut decoded)?;
            let mut pending = pending.as_mut().context("a null pending mask")?.mask()?;
            let mut inserted = inserted.as_mut().context("a null inserted mask")?.mask()?;
            let nrows = pending.as_view().nrows();
            let hashes = values(hashes, nrows, "hashes")?;
            let offsets = slots(offsets, nrows, "offsets")?;
            table
                .find_or_insert(hashes, &decoded, &mut pending, offsets, &mut inserted)
                .map(drop)
        })
    }
}

/// `tess_table_insert_grouped`: insert each pending row right after a
/// record with the same keys, when the table holds one.
///
/// # Safety
///
/// `region` as for [`TableMut::attach_mut`] during the call; `keys` as
/// for [`table_keys`]; `pending` and `duplicates` must point to valid
/// masks that nothing else accesses, with the batch's rows; `hashes` must
/// hold a hash per row, `offsets` a writable slot per row, and `payload`
/// be null or hold the payload of every row; `status` as for every entry
/// point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_insert_grouped(
    region: *mut u8,
    len: usize,
    hashes: *const u32,
    nkeys: c_int,
    keys: *const TableKey,
    payload: *const u8,
    pending: *mut Mask,
    offsets: *mut u32,
    duplicates: *mut Mask,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let mut table = TableMut::attach_mut(region, len)?;
            let mut decoded = TableKeys::empty();
            table_keys(nkeys, keys, &mut decoded)?;
            let mut pending = pending.as_mut().context("a null pending mask")?.mask()?;
            let mut duplicates = duplicates
                .as_mut()
                .context("a null duplicates mask")?
                .mask()?;
            let nrows = pending.as_view().nrows();
            let hashes = values(hashes, nrows, "hashes")?;
            let offsets = slots(offsets, nrows, "offsets")?;
            let payload = if payload.is_null() {
                None
            } else {
                let bytes = nrows
                    .checked_mul(table.payload_size())
                    .context("the payload does not fit in memory")?;
                Some(values(payload, bytes, "payload")?)
            };
            table
                .insert_grouped(
                    hashes,
                    &decoded,
                    payload,
                    &mut pending,
                    offsets,
                    &mut duplicates,
                )
                .map(drop)
        })
    }
}

/// `tess_table_payload`: the payload of a record, to change in place.
///
/// # Safety
///
/// `region` as for [`TableMut::attach_mut`] during the call and until the
/// caller is done with the pointer it receives; `payload` must be null or
/// writable; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_payload(
    region: *mut u8,
    len: usize,
    offset: u32,
    payload: *mut *mut u8,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let mut table = TableMut::attach_mut(region, len)?;
            let bytes = table.payload_mut(offset)?.as_mut_ptr();
            *payload.as_mut().context("a null payload pointer")? = bytes;
            Ok(())
        })
    }
}

/// `tess_table_scan`: the next records in insertion order.
///
/// # Safety
///
/// `region` as for [`TableMut::attach_mut`] during the call; `cursor` and
/// `count` must be null or writable; `offsets` must hold `capacity`
/// writable slots; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_scan(
    region: *mut u8,
    len: usize,
    cursor: *mut u64,
    offsets: *mut u32,
    capacity: c_int,
    count: *mut c_int,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let table = TableMut::attach_mut(region, len)?;
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

/// `tess_table_grow`: grow the table to the whole region.
///
/// # Safety
///
/// `region` as for [`TableMut::attach_mut`] during the call, the first
/// bytes of it holding what the table held; `status` as for every entry
/// point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_grow(region: *mut u8, len: usize, status: *mut Status) -> Code {
    // SAFETY: the caller's contract.
    unsafe { guard(status, || TableMut::attach_mut(region, len)?.grow(len)) }
}

/// `TessTableAccumulate`: how `tess_table_accumulate` updates a state.
const ACCUMULATE_COUNT_ROWS: c_uint = 1;
const ACCUMULATE_COUNT: c_uint = 2;
const ACCUMULATE_SUM_INT4: c_uint = 3;
const ACCUMULATE_MIN_INT4: c_uint = 4;
const ACCUMULATE_MAX_INT4: c_uint = 5;
const ACCUMULATE_MIN_INT8: c_uint = 6;
const ACCUMULATE_MAX_INT8: c_uint = 7;

/// `tess_table_accumulate`: fold each selected row into the aggregate
/// state of its record's payload.
///
/// # Safety
///
/// `region` as for [`TableMut::attach_mut`] during the call; `rows` must
/// point to a valid mask; `offsets` must hold an initialized offset per
/// row; `column` is ignored for `count(*)` and otherwise must satisfy
/// [`DatumColumn::ints`]'s contract with `prepared` as its readiness;
/// `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_accumulate(
    region: *mut u8,
    len: usize,
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
            let mut table = TableMut::attach_mut(region, len)?;
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
                ACCUMULATE_MIN_INT8 | ACCUMULATE_MAX_INT8 => {
                    let column = reader::<i64>(column, prepared)?;
                    let fold = if op == ACCUMULATE_MIN_INT8 {
                        Fold::Min
                    } else {
                        Fold::Max
                    };
                    table.fold(offsets, &rows, &column, fold, slot)
                }
                other => bail!("unknown accumulation {other}"),
            }
        })
    }
}

/// `tess_table_gather_key`: one key of each selected row's record, as its
/// Datum and NULL flag.
///
/// # Safety
///
/// `region` as for [`Table::attach`] during the call; `rows` must point
/// to a valid mask; `offsets` must hold an initialized offset per row,
/// `values` a writable Datum and `isnull` a writable flag per row that
/// nothing else accesses; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_gather_key(
    region: *const u8,
    len: usize,
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
            let table = Table::attach(region.cast_mut(), len)?;
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
/// `region` as for [`Table::attach`] during the call, with no insertion
/// running; `words` must point to `nwords` writable words that nothing
/// else accesses; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_bloom(
    region: *const u8,
    len: usize,
    words: *mut u64,
    nwords: usize,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let table = Table::attach(region.cast_mut(), len)?;
            let words = slots(words, nwords, "filter words")?;
            table.bloom(words)
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

/// `tess_table_stage`: write the pending rows of a batch as the table's
/// records into a staging buffer, as long as whole records fit.
///
/// # Safety
///
/// `region` as for [`Table::attach`] during the call; `buffer` must point
/// to `nwords` writable words that nothing else accesses and `used` to
/// their used byte count; `keys`, `hashes`, `payload` and `pending` as for
/// [`tess_table_insert`]; `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_stage(
    region: *const u8,
    len: usize,
    buffer: *mut u64,
    nwords: usize,
    used: *mut usize,
    hashes: *const u32,
    nkeys: c_int,
    keys: *const TableKey,
    payload: *const u8,
    pending: *mut Mask,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let table = Table::attach(region.cast_mut(), len)?;
            let mut decoded = TableKeys::empty();
            table_keys(nkeys, keys, &mut decoded)?;
            let mut pending = pending.as_mut().context("a null pending mask")?.mask()?;
            let nrows = pending.as_view().nrows();
            let hashes = values(hashes, nrows, "hashes")?;
            let buffer = slots(buffer, nwords, "staging buffer")?;
            let used = used.as_mut().context("a null used count")?;
            let payload = if payload.is_null() {
                None
            } else {
                let bytes = nrows
                    .checked_mul(table.payload_size())
                    .context("the payload does not fit in memory")?;
                Some(values(payload, bytes, "payload")?)
            };
            table
                .stage(buffer, used, hashes, &decoded, payload, &mut pending)
                .map(drop)
        })
    }
}

/// `tess_table_insert_staged`: add a staging buffer's records to the
/// table, as long as it has room.
///
/// # Safety
///
/// `region` as for [`Table::attach`] during the call, which may run with
/// other insertions; `buffer` must point to initialized words holding at
/// least `used` bytes, and `consumed` to the count of them already added;
/// `status` as for every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_insert_staged(
    region: *mut u8,
    len: usize,
    buffer: *const u64,
    used: usize,
    consumed: *mut usize,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let table = Table::attach(region, len)?;
            let buffer = values(buffer, used.div_ceil(8), "staging buffer")?;
            let consumed = consumed.as_mut().context("a null consumed count")?;
            table.insert_staged(buffer, used, consumed).map(drop)
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
/// `region` as for [`Table::attach`] during the call, with no insertion
/// running; `words` as for [`tess_bloom_shared_init`], other participants
/// using it too; `built` must point to a writable flag; `status` as for
/// every entry point.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_table_try_build_bloom(
    region: *const u8,
    len: usize,
    words: *mut u64,
    nwords: usize,
    built: *mut bool,
    status: *mut Status,
) -> Code {
    // SAFETY: the caller's contract.
    unsafe {
        guard(status, || {
            let table = Table::attach(region.cast_mut(), len)?;
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

#[cfg(test)]
mod tests {
    use super::{TableKey, TableRecord, TableStats};

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
