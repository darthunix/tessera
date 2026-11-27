//! The C entry points of the reader, declared in
//! `parquet/tessera_parquet_reader.h`.
//!
//! Every entry point returns `true` on success and `false` with its status
//! filled on an error; PostgreSQL raises the error after the call has
//! returned, so that no Rust frame is on the stack when its error unwinds.
//! Each runs inside [`std::panic::catch_unwind`]: a panic while a file is
//! decoded is reported as bytes the format cannot decode, `XX001`, any
//! other as an internal error, `XX000`. The reader calls no function of
//! PostgreSQL and allocates its own objects, which the caller frees with
//! the entry points of their type; memory the caller gives it, such as the
//! arena of a column's text, it only fills.

use std::ffi::c_char;
use std::os::unix::ffi::OsStrExt;
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::sync::Arc;

use arrow_array::cast::AsArray;
use arrow_array::{Array, ArrayRef};

use datum_arrow::{Comparison, Failures, MAX_VALUE_BYTES, ReadOutput, TextBytes, Unit, mask_words};

use crate::bytes::{Slots, mark_nulls};
use crate::clause::{ClauseKind, ClauseTest, comparison, like_checked};
use crate::column::{ColumnRequest, MappedColumn, map_column};
use crate::error::{DATA_CORRUPTED, Error, INTERNAL_ERROR, Result};
use crate::file::{Identity, ParquetFile};
use crate::reader::{ColumnReader, Window, input_of};

// The headers of `text` are written as a little-endian machine reads them.
#[cfg(not(target_endian = "little"))]
compile_error!("tessera-parquet writes the headers of text for a little-endian machine");

/// Bytes of the message of a status, its terminator included.
pub const MESSAGE_SIZE: usize = 256;

/// The status of a call that failed, as `TessParquetStatus`.
#[repr(C)]
#[derive(Debug)]
pub struct Status {
    /// A five-character SQLSTATE and its terminator.
    pub sqlstate: [c_char; 6],
    /// The row of the value that failed, or -1.
    pub row: i64,
    /// A NUL-terminated message for people.
    pub message: [c_char; MESSAGE_SIZE],
}

/// What `fstat` said of a file, as `TessParquetIdentity`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CIdentity {
    /// The size in bytes.
    pub size: u64,
    /// The time of modification, seconds since 1970.
    pub mtime_sec: i64,
    /// Its nanoseconds.
    pub mtime_nsec: i64,
    /// The device.
    pub dev: u64,
    /// The inode.
    pub ino: u64,
}

impl From<Identity> for CIdentity {
    fn from(i: Identity) -> Self {
        CIdentity {
            size: i.size,
            mtime_sec: i.mtime_sec,
            mtime_nsec: i.mtime_nsec,
            dev: i.dev,
            ino: i.ino,
        }
    }
}

impl From<CIdentity> for Identity {
    fn from(i: CIdentity) -> Self {
        Identity {
            size: i.size,
            mtime_sec: i.mtime_sec,
            mtime_nsec: i.mtime_nsec,
            dev: i.dev,
            ino: i.ino,
        }
    }
}

/// An open file, as `TessParquetFile`.
pub struct FileHandle(ParquetFile);

/// A mapped column, as `TessParquetColumn`.
pub struct ColumnHandle(MappedColumn);

/// A reader of a column of a group, and the window it read last, as
/// `TessParquetReader`.
pub struct ReaderHandle {
    reader: ColumnReader,
    window: Option<Window>,
    /// The text the last call of `tess_parquet_text_size` checked, for the
    /// window still held; none after a read or a check that failed.
    text: Option<SizedText>,
    /// The dictionary values array [`tess_parquet_bytes`] numbered last,
    /// held so that no other array takes its addresses, and its number;
    /// and the numbers given so far.
    numbered: Option<(ArrayRef, u64)>,
    numbers: u64,
    /// Whether every value of the array numbered last passed the column's
    /// rule, as one read of the whole array found, and none is `NULL`:
    /// unknown before a window names it.
    dictionary_passes: Option<bool>,
    /// Whether the last window, a plain one, passed its check value by
    /// value: unknown before a window is asked, and again after each read.
    window_passes: Option<bool>,
    /// The values of dictionaries checked whole and the plain windows
    /// checked, once a window, which the tests read.
    values_checked: usize,
    windows_checked: usize,
    /// The mask of the `NULL` rows of the last ask of
    /// [`tess_parquet_bytes`] or [`tess_parquet_values`], kept for the
    /// next one, which reuses it while its capacity suffices; an ask that
    /// fails once it took it leaves none, and the next allocates one.
    made: Vec<u64>,
    /// Every window read row by row through the plan, for the tests.
    #[cfg(test)]
    rows_only: bool,
    /// The longest value the cheaper check gives, for the tests: a longer
    /// one sends its window to the read row by row.
    #[cfg(test)]
    max_fast: usize,
}

/// The places of a window's text in its own buffers, a row each, `None`
/// for a row not asked or `NULL`, and the bytes of an arena they need.
struct SizedText {
    places: Vec<Option<(*const u8, usize)>>,
    bytes: usize,
}

/// Runs `body`, writing its error or a caught panic into `status`.
///
/// # Safety
///
/// `status` must be null or point to a writable `Status`.
unsafe fn guard(status: *mut Status, decoding: bool, body: impl FnOnce() -> Result<()>) -> bool {
    // The body borrows only what the caller gave and what it owns; after a
    // panic those are freed or reused by the caller only through the entry
    // points, which check their state.
    let outcome = panic::catch_unwind(AssertUnwindSafe(body));
    let error = match outcome {
        Ok(Ok(())) => return true,
        Ok(Err(error)) => error,
        Err(payload) => {
            let text = payload
                .downcast_ref::<&str>()
                .map(|t| (*t).to_owned())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| String::from("a panic"));
            let code = if decoding {
                DATA_CORRUPTED
            } else {
                INTERNAL_ERROR
            };
            Error::new(code, format!("Parquet reader failed: {text}"))
        }
    };
    if !status.is_null() {
        // SAFETY: the caller gives null or a status it may not have
        // initialized; each field is written whole, none read.
        unsafe { fill(status, &error) };
    }
    false
}

/// Writes `error` into the status at `status`, field by field, through
/// the raw pointer: the caller may give memory it has not initialized.
///
/// # Safety
///
/// `status` must point to writable memory of a `Status`.
unsafe fn fill(status: *mut Status, error: &Error) {
    let mut sqlstate = [0 as c_char; 6];
    for (place, byte) in sqlstate.iter_mut().zip(error.sqlstate().bytes()) {
        *place = byte as c_char;
    }
    let row = error
        .row()
        .and_then(|r| i64::try_from(r).ok())
        .unwrap_or(-1);
    let mut message = [0 as c_char; MESSAGE_SIZE];
    let text = error.message();
    // Cut at a character's boundary, so that the message stays UTF-8.
    let mut len = text.len().min(MESSAGE_SIZE - 1);
    while !text.is_char_boundary(len) {
        len -= 1;
    }
    for (place, &byte) in message.iter_mut().zip(&text.as_bytes()[..len]) {
        *place = byte as c_char;
    }
    // SAFETY: the caller gives a writable status; the places of its
    // fields are written, no reference to it is made.
    unsafe {
        std::ptr::addr_of_mut!((*status).sqlstate).write(sqlstate);
        std::ptr::addr_of_mut!((*status).row).write(row);
        std::ptr::addr_of_mut!((*status).message).write(message);
    }
}

/// Writes `value` into the output at `out`, which the caller may not have
/// initialized.
///
/// # Safety
///
/// `out` must be null or point to writable memory of a `T`.
unsafe fn put<T>(out: *mut T, value: T) -> Result<()> {
    if out.is_null() {
        return Err(broken("a null output"));
    }
    // SAFETY: the caller gives a writable place; nothing is read from it.
    unsafe { out.write(value) };
    Ok(())
}

fn broken(message: &str) -> Error {
    Error::new(INTERNAL_ERROR, message)
}

/// # Safety
///
/// `ptr` must be null with `len` 0, or point to `len` readable bytes.
unsafe fn bytes<'a>(ptr: *const c_char, len: usize) -> Result<&'a [u8]> {
    if ptr.is_null() {
        return if len == 0 {
            Ok(&[])
        } else {
            Err(broken("a null pointer with a length"))
        };
    }
    // SAFETY: the caller gives `len` readable bytes at `ptr`.
    Ok(unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), len) })
}

/// # Safety
///
/// `ptr` must be null with `len` 0, or point to `len` readable words.
unsafe fn words<'a>(ptr: *const u64, len: usize) -> Result<&'a [u64]> {
    if ptr.is_null() {
        return if len == 0 {
            Ok(&[])
        } else {
            Err(broken("a null pointer with a length"))
        };
    }
    // SAFETY: the caller gives `len` readable words at `ptr`.
    Ok(unsafe { std::slice::from_raw_parts(ptr, len) })
}

/// # Safety
///
/// `ptr` must be null with `len` 0, or point to `len` writable words.
unsafe fn words_mut<'a>(ptr: *mut u64, len: usize) -> Result<&'a mut [u64]> {
    if ptr.is_null() {
        return if len == 0 {
            Ok(&mut [])
        } else {
            Err(broken("a null pointer with a length"))
        };
    }
    // SAFETY: the caller gives `len` writable words at `ptr`.
    Ok(unsafe { std::slice::from_raw_parts_mut(ptr, len) })
}

/// Opens the file at the path of `path_len` bytes and reads its footer,
/// through the backend's cache of footers; where `expected` is not null,
/// refuses a file of another identity before its footer is read.
///
/// # Safety
///
/// `path` must point to `path_len` readable bytes; `expected` must be null
/// or point to an identity; `out` must point to a writable pointer;
/// `status` as [`guard`] says.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_parquet_open(
    path: *const c_char,
    path_len: usize,
    expected: *const CIdentity,
    out: *mut *mut FileHandle,
    status: *mut Status,
) -> bool {
    // SAFETY: as the function's contract says.
    unsafe {
        guard(status, true, || {
            let path = bytes(path, path_len)?;
            let path = Path::new(std::ffi::OsStr::from_bytes(path));
            if out.is_null() {
                return Err(broken("a null output"));
            }
            let expected = expected.as_ref().map(|identity| Identity::from(*identity));
            let file = ParquetFile::open_cached(path, expected.as_ref())?;
            put(out, Box::into_raw(Box::new(FileHandle(file))))
        })
    }
}

/// Frees a file opened by [`tess_parquet_open`]; its descriptor is closed
/// once no reader of it is left.
///
/// # Safety
///
/// `file` must be null or a file of [`tess_parquet_open`] not yet freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_parquet_file_free(file: *mut FileHandle) {
    if !file.is_null() {
        // SAFETY: a file of `tess_parquet_open`, freed once.
        drop(unsafe { Box::from_raw(file) });
    }
}

/// Writes what `fstat` said of `file` when it was opened, its count of
/// groups of rows and its rows.
///
/// # Safety
///
/// `file` must be a live file; the outputs must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_parquet_file_info(
    file: *const FileHandle,
    identity: *mut CIdentity,
    groups: *mut usize,
    rows: *mut u64,
    status: *mut Status,
) -> bool {
    // SAFETY: as the function's contract says.
    unsafe {
        guard(status, false, || {
            let file = &file.as_ref().ok_or_else(|| broken("a null file"))?.0;
            if identity.is_null() || groups.is_null() || rows.is_null() {
                return Err(broken("a null output"));
            }
            put(identity, file.identity().into())?;
            put(groups, file.groups())?;
            put(rows, file.rows())
        })
    }
}

/// Writes the rows of group `group` of `file`.
///
/// # Safety
///
/// `file` must be a live file; `rows` must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_parquet_group_rows(
    file: *const FileHandle,
    group: usize,
    rows: *mut u64,
    status: *mut Status,
) -> bool {
    // SAFETY: as the function's contract says.
    unsafe {
        guard(status, false, || {
            let file = &file.as_ref().ok_or_else(|| broken("a null file"))?.0;
            let count = file
                .group_rows(group)
                .ok_or_else(|| broken("a group of rows past the file's"))?;
            put(rows, count)
        })
    }
}

/// Checks that `file` still has the size and time it had when opened.
///
/// # Safety
///
/// `file` must be a live file.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_parquet_check(file: *const FileHandle, status: *mut Status) -> bool {
    // SAFETY: as the function's contract says.
    unsafe {
        guard(status, false, || {
            file.as_ref()
                .ok_or_else(|| broken("a null file"))?
                .0
                .check_unchanged()
        })
    }
}

/// Maps a column of a foreign table to a column of `file`: the file
/// column's name of `name_len` bytes, the declared type and modifier, the
/// option `unit` (-1 for none; 0 days, 1 seconds, 2 milliseconds, 3
/// microseconds), the option `utf8` (-1 for none, 0 `check`, 1 `trust`),
/// and whether the database's encoding is UTF-8.
///
/// # Safety
///
/// `file` must be a live file; `name` must point to `name_len` readable
/// bytes; `out` must point to a writable pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_parquet_column(
    file: *const FileHandle,
    name: *const c_char,
    name_len: usize,
    type_oid: u32,
    typmod: i32,
    unit: i32,
    utf8: i32,
    server_utf8: bool,
    out: *mut *mut ColumnHandle,
    status: *mut Status,
) -> bool {
    // SAFETY: as the function's contract says.
    unsafe {
        guard(status, false, || {
            let file = &file.as_ref().ok_or_else(|| broken("a null file"))?.0;
            let name = std::str::from_utf8(bytes(name, name_len)?)
                .map_err(|_| broken("a column name that is not UTF-8"))?;
            let unit = match unit {
                -1 => None,
                0 => Some(Unit::Days),
                1 => Some(Unit::Seconds),
                2 => Some(Unit::Milliseconds),
                3 => Some(Unit::Microseconds),
                _ => return Err(broken("an unknown unit")),
            };
            let utf8 = match utf8 {
                -1 => None,
                0 => Some(TextBytes::Checked),
                1 => Some(TextBytes::Trusted),
                _ => return Err(broken("an unknown treatment of text")),
            };
            let request = ColumnRequest {
                name,
                type_oid,
                typmod,
                unit,
                utf8,
                server_utf8,
            };
            if out.is_null() {
                return Err(broken("a null output"));
            }
            let column = map_column(file, &request)?;
            put(out, Box::into_raw(Box::new(ColumnHandle(column))))
        })
    }
}

/// What the wrapper needs of a mapped column, as `TessParquetColumnInfo`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ColumnInfo {
    /// Whether its values are `Datum` words of a type passed by value, by
    /// [`tess_parquet_values`]; text and bytes otherwise, by
    /// [`tess_parquet_text_size`] and [`tess_parquet_text`].
    pub gives_datums: bool,
    /// Whether the file column may hold `NULL`.
    pub nullable: bool,
    /// The bytes of the column's chunks in the file, compressed.
    pub bytes: u64,
}

/// Writes what the wrapper needs of `column`.
///
/// # Safety
///
/// `column` must be live; `info` must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_parquet_column_info(
    column: *const ColumnHandle,
    info: *mut ColumnInfo,
    status: *mut Status,
) -> bool {
    // SAFETY: as the function's contract says.
    unsafe {
        guard(status, false, || {
            let column = &column.as_ref().ok_or_else(|| broken("a null column"))?.0;
            put(
                info,
                ColumnInfo {
                    gives_datums: column.plan().gives_datums(),
                    nullable: column.nullable(),
                    bytes: column.chunk_bytes(),
                },
            )
        })
    }
}

/// Frees a column of [`tess_parquet_column`].
///
/// # Safety
///
/// `column` must be null or a column not yet freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_parquet_column_free(column: *mut ColumnHandle) {
    if !column.is_null() {
        // SAFETY: a column of `tess_parquet_column`, freed once.
        drop(unsafe { Box::from_raw(column) });
    }
}

/// Makes a reader of `column` in group `group` of `file` for the rows of
/// the selection of `words` words, a bit for each row of the group; its
/// arrays hold `batch` selected rows.
///
/// # Safety
///
/// `file` and `column` must be live and of the same file; `selection`
/// must point to `words` readable words; `out` to a writable pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_parquet_reader(
    file: *const FileHandle,
    column: *const ColumnHandle,
    group: usize,
    selection: *const u64,
    selection_words: usize,
    batch: usize,
    out: *mut *mut ReaderHandle,
    status: *mut Status,
) -> bool {
    // SAFETY: as the function's contract says.
    unsafe {
        guard(status, false, || {
            let file = &file.as_ref().ok_or_else(|| broken("a null file"))?.0;
            let column = &column.as_ref().ok_or_else(|| broken("a null column"))?.0;
            let selection: Arc<[u64]> = words(selection, selection_words)?.into();
            if out.is_null() {
                return Err(broken("a null output"));
            }
            let reader = ColumnReader::new(file, column, group, selection, batch)?;
            put(
                out,
                Box::into_raw(Box::new(ReaderHandle {
                    reader,
                    window: None,
                    text: None,
                    numbered: None,
                    numbers: 0,
                    dictionary_passes: None,
                    window_passes: None,
                    values_checked: 0,
                    windows_checked: 0,
                    made: Vec::new(),
                    #[cfg(test)]
                    rows_only: false,
                    #[cfg(test)]
                    max_fast: MAX_VALUE_BYTES,
                })),
            )
        })
    }
}

/// Frees a reader of [`tess_parquet_reader`].
///
/// # Safety
///
/// `reader` must be null or a reader not yet freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_parquet_reader_free(reader: *mut ReaderHandle) {
    if !reader.is_null() {
        // SAFETY: a reader of `tess_parquet_reader`, freed once.
        drop(unsafe { Box::from_raw(reader) });
    }
}

/// Reads the window of `len` rows from the group's row `start`, for the
/// rows of the mask `rows` of `rows_words` words over the window, as
/// [`ColumnReader::read`] says; writes the mask of the rows the window
/// holds into `held`, of `held_words` words, and their count into
/// `count`. The window is the reader's until the next read.
///
/// # Safety
///
/// `reader` must be live; `rows` must point to `rows_words` readable
/// words and `held` to `held_words` writable ones, which the caller has
/// initialized; `count` must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_parquet_read(
    reader: *mut ReaderHandle,
    start: usize,
    len: usize,
    rows: *const u64,
    rows_words: usize,
    held: *mut u64,
    held_words: usize,
    count: *mut usize,
    status: *mut Status,
) -> bool {
    // SAFETY: as the function's contract says.
    unsafe {
        guard(status, true, || {
            let handle = reader.as_mut().ok_or_else(|| broken("a null reader"))?;
            let rows = words(rows, rows_words)?;
            let held = words_mut(held, held_words)?;
            if held.len() < mask_words(len) || count.is_null() {
                return Err(broken("a mask of rows held shorter than the window"));
            }
            handle.window = None;
            handle.text = None;
            let window = handle.reader.read(start, len, rows)?;
            held[..window.rows.len()].copy_from_slice(&window.rows);
            put(count, window.array.len())?;
            handle.window = Some(window);
            handle.window_passes = None;
            Ok(())
        })
    }
}

/// Sets in `nulls` the bits of the rows `asked` asks that `made` gives as
/// `NULL`, and clears those of the other rows asked; the bits of rows not
/// asked stay as an earlier call left them.
fn merge_nulls(nulls: &mut [u64], made: &[u64], asked: &[u64]) {
    for ((place, made), asked) in nulls.iter_mut().zip(made).zip(asked) {
        *place = (*place & !asked) | (made & asked);
    }
}

/// Converts the rows of the last window that `asked`, a mask over its rows
/// held in their order, asks for, into `Datum` words: `values` of at least
/// the window's held rows, and the bits of those rows in `nulls`, a mask
/// of as many, whose bits of rows not asked stay as they were. The place
/// in `values` of a row not asked may be written, as `ReadPlan::read` of
/// `datum-arrow` says for a type passed by value: with the row's own value
/// where it converts, and otherwise with a value of no meaning.
///
/// # Safety
///
/// `reader` must be live; `asked` must point to `asked_words` readable
/// words, `values` and `nulls` to as many writable words as their lengths
/// say, initialized by the caller; none of the three may overlap.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_parquet_values(
    reader: *mut ReaderHandle,
    asked: *const u64,
    asked_words: usize,
    values: *mut u64,
    values_len: usize,
    nulls: *mut u64,
    nulls_words: usize,
    status: *mut Status,
) -> bool {
    // SAFETY: as the function's contract says.
    unsafe {
        guard(status, false, || {
            let handle = reader.as_mut().ok_or_else(|| broken("a null reader"))?;
            let window = handle
                .window
                .as_ref()
                .ok_or_else(|| broken("no window read"))?;
            let asked = words(asked, asked_words)?;
            let nulls = words_mut(nulls, nulls_words)?;
            let values = words_mut(values, values_len)?;
            let mut buffers = Vec::new();
            let input = input_of(window.array.as_ref(), &mut buffers)?;
            // The reader's own mask of the rows asked that are NULL, of the
            // caller's length, so that a short one is refused as before.
            let mut made = std::mem::take(&mut handle.made);
            made.clear();
            made.resize(nulls.len(), 0);
            let output = &mut ReadOutput::Datums {
                values,
                nulls: &mut made,
            };
            handle.reader.plan().read(&input, asked, output)?;
            merge_nulls(nulls, &made, asked);
            handle.made = made;
            Ok(())
        })
    }
}

/// A clause's test of a column's stored values, as `TessParquetTest`.
pub struct TestHandle(ClauseTest);

/// The kinds of a test: the comparisons in the order of [`Comparison`],
/// then `IN`, `IS NULL` and `IS NOT NULL`, then the tests of strings: `=`,
/// `<>`, `LIKE` and `NOT LIKE`.
const TEST_KINDS: [ClauseKind; 13] = [
    ClauseKind::Compare(Comparison::Eq),
    ClauseKind::Compare(Comparison::Ne),
    ClauseKind::Compare(Comparison::Lt),
    ClauseKind::Compare(Comparison::Le),
    ClauseKind::Compare(Comparison::Gt),
    ClauseKind::Compare(Comparison::Ge),
    ClauseKind::In,
    ClauseKind::IsNull,
    ClauseKind::IsNotNull,
    ClauseKind::BytesEq,
    ClauseKind::BytesNe,
    ClauseKind::Like,
    ClauseKind::NotLike,
];

/// Whether the source checks `LIKE` with the `len` bytes of `pattern` on a
/// string's bytes, as [`like_checked`] says.
///
/// # Safety
///
/// `pattern` must point to `len` readable bytes, or be null with `len` 0.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_parquet_like_checked(pattern: *const u8, len: usize) -> bool {
    if pattern.is_null() {
        return len == 0 && like_checked(&[]);
    }
    // SAFETY: as the function's contract says.
    like_checked(unsafe { std::slice::from_raw_parts(pattern, len) })
}

/// Makes the test of `column`'s strings that keeps a clause of the kind
/// `kind`, one of the tests of strings, with the `len` bytes of `value`,
/// or `NULL` where `isnull`, as [`ClauseTest::of_bytes`] takes them.
///
/// # Safety
///
/// `column` must be live; `value` must point to `len` readable bytes, or
/// be null with `len` 0, unless `isnull`; `out` must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_parquet_bytes_test(
    column: *const ColumnHandle,
    kind: i32,
    value: *const u8,
    len: usize,
    isnull: bool,
    out: *mut *mut TestHandle,
    status: *mut Status,
) -> bool {
    // SAFETY: as the function's contract says.
    unsafe {
        guard(status, false, || {
            let column = &column.as_ref().ok_or_else(|| broken("a null column"))?.0;
            let kind = usize::try_from(kind)
                .ok()
                .and_then(|kind| TEST_KINDS.get(kind))
                .ok_or_else(|| broken("an unknown kind of test"))?;
            let bytes = match (isnull, value.is_null()) {
                (true, _) => None,
                (false, true) if len == 0 => Some(&[][..]),
                (false, true) => return Err(broken("null bytes")),
                (false, false) => Some(std::slice::from_raw_parts(value, len)),
            };
            if out.is_null() {
                return Err(broken("a null output"));
            }
            let test = ClauseTest::of_bytes(column, *kind, bytes)?;
            put(out, Box::into_raw(Box::new(TestHandle(test))))
        })
    }
}

/// The kind of test, `TESS_PARQUET_TEST_EQ` to `TESS_PARQUET_TEST_GE`, of
/// a clause that compares a column of PostgreSQL type `column` with a
/// side of type `side`, as [`comparison`] takes them, or -1 for a clause
/// the source does not check.
#[unsafe(no_mangle)]
pub extern "C" fn tess_parquet_comparison(
    column: u32,
    side: u32,
    strategy: i32,
    negator: i32,
    commuted: bool,
) -> i32 {
    comparison(column, side, strategy, negator, commuted)
        .and_then(|op| {
            TEST_KINDS
                .iter()
                .position(|kind| *kind == ClauseKind::Compare(op))
        })
        .and_then(|kind| i32::try_from(kind).ok())
        .unwrap_or(-1)
}

/// Makes the test of `column`'s stored values that keeps a clause of the
/// kind `kind` with the `nvalues` values `values`, each `NULL` where
/// `nulls` says, as [`ClauseTest::new`] takes them.
///
/// # Safety
///
/// `column` must be live; `values` and `nulls` must point to `nvalues`
/// readable items, or be null with `nvalues` 0; `out` must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_parquet_test(
    column: *const ColumnHandle,
    kind: i32,
    values: *const i64,
    nulls: *const bool,
    nvalues: usize,
    out: *mut *mut TestHandle,
    status: *mut Status,
) -> bool {
    // SAFETY: as the function's contract says.
    unsafe {
        guard(status, false, || {
            let column = &column.as_ref().ok_or_else(|| broken("a null column"))?.0;
            let kind = usize::try_from(kind)
                .ok()
                .and_then(|kind| TEST_KINDS.get(kind))
                .ok_or_else(|| broken("an unknown kind of test"))?;
            if nvalues > 0 && (values.is_null() || nulls.is_null()) {
                return Err(broken("null values"));
            }
            let given: Vec<Option<i64>> = (0..nvalues)
                .map(|i| (!*nulls.add(i)).then(|| *values.add(i)))
                .collect();
            if out.is_null() {
                return Err(broken("a null output"));
            }
            let test = ClauseTest::new(column, *kind, &given)?;
            put(out, Box::into_raw(Box::new(TestHandle(test))))
        })
    }
}

/// Sets `out` where group `group` of `file` holds no row that passes
/// `test`, by what the footer says of `column`'s chunk: its statistics,
/// or, with `dictionary`, its dictionary read alone, as
/// [`ClauseTest::rules_out_group`] says.
///
/// # Safety
///
/// `file`, `column` and `test` must be live, the column mapped in the file
/// and the test made for it; `out` must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_parquet_group_ruled_out(
    file: *const FileHandle,
    column: *const ColumnHandle,
    group: usize,
    test: *const TestHandle,
    dictionary: bool,
    out: *mut bool,
    status: *mut Status,
) -> bool {
    // SAFETY: as the function's contract says.
    unsafe {
        guard(status, false, || {
            let file = &file.as_ref().ok_or_else(|| broken("a null file"))?.0;
            let column = &column.as_ref().ok_or_else(|| broken("a null column"))?.0;
            let test = &test.as_ref().ok_or_else(|| broken("a null test"))?.0;
            if out.is_null() {
                return Err(broken("a null output"));
            }
            let ruled_out = test.rules_out_group(file, column, group, dictionary)?;
            put(out, ruled_out)
        })
    }
}

/// Frees a test of [`tess_parquet_test`].
///
/// # Safety
///
/// `test` must be null or a test not yet freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_parquet_test_free(test: *mut TestHandle) {
    if !test.is_null() {
        // SAFETY: a test of `tess_parquet_test`, freed once.
        drop(unsafe { Box::from_raw(test) });
    }
}

/// Writes into `passed`, of `passed_words` words, a bit for each row the
/// reader's last window holds, in their order, set where the row passes
/// `test`, as [`ClauseTest::apply`] says.
///
/// # Safety
///
/// `reader` and `test` must be live; `passed` must point to
/// `passed_words` writable words.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_parquet_window_test(
    reader: *const ReaderHandle,
    test: *const TestHandle,
    passed: *mut u64,
    passed_words: usize,
    status: *mut Status,
) -> bool {
    // SAFETY: as the function's contract says.
    unsafe {
        guard(status, false, || {
            let handle = reader.as_ref().ok_or_else(|| broken("a null reader"))?;
            let test = &test.as_ref().ok_or_else(|| broken("a null test"))?.0;
            let window = handle
                .window
                .as_ref()
                .ok_or_else(|| broken("no window read"))?;
            test.apply(window.array.as_ref(), words_mut(passed, passed_words)?)
        })
    }
}

/// The bytes of a short header, 1, where a value of `len` bytes fits one.
const SHORT_MAX: usize = 126;

/// The bytes the text of `values` takes in an arena that starts aligned
/// to 4 bytes: a short header of one byte for a value of up to 126 bytes,
/// and an aligned header of four bytes otherwise.
fn arena_bytes(values: &[Option<(*const u8, usize)>]) -> usize {
    let mut at = 0usize;
    for (_, len) in values.iter().flatten() {
        if *len <= SHORT_MAX {
            at += 1 + len;
        } else {
            at = at.next_multiple_of(4) + 4 + len;
        }
    }
    at
}

/// Checks the text of the rows `asked` asks of the last window, as the
/// column's read plan says; writes their bits of `NULL` into `nulls`, as
/// [`tess_parquet_values`] does, and the bytes of the arena their values
/// need into `bytes`, for [`tess_parquet_text`].
///
/// # Safety
///
/// `reader` must be live; `asked` must point to `asked_words` readable
/// words and `nulls` to `nulls_words` writable ones, initialized by the
/// caller and not overlapping; `bytes` must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_parquet_text_size(
    reader: *mut ReaderHandle,
    asked: *const u64,
    asked_words: usize,
    nulls: *mut u64,
    nulls_words: usize,
    bytes: *mut usize,
    status: *mut Status,
) -> bool {
    // SAFETY: as the function's contract says.
    unsafe {
        guard(status, false, || {
            let handle = reader.as_mut().ok_or_else(|| broken("a null reader"))?;
            handle.text = None;
            let asked = words(asked, asked_words)?;
            let nulls = words_mut(nulls, nulls_words)?;
            if bytes.is_null() {
                return Err(broken("a null output"));
            }
            let window = handle
                .window
                .as_ref()
                .ok_or_else(|| broken("no window read"))?;
            let len = window.array.len();
            let mut buffers = Vec::new();
            let input = input_of(window.array.as_ref(), &mut buffers)?;
            let mut values: Vec<&[u8]> = vec![&[]; len];
            let mut made = vec![0u64; nulls.len()];
            handle.reader.plan().read(
                &input,
                asked,
                &mut ReadOutput::Bytes {
                    values: &mut values,
                    nulls: &mut made,
                },
            )?;
            let places: Vec<Option<(*const u8, usize)>> = (0..len)
                .map(|row| {
                    let wanted = asked[row / 64] >> (row % 64) & 1 == 1
                        && made[row / 64] >> (row % 64) & 1 == 0;
                    wanted.then(|| (values[row].as_ptr(), values[row].len()))
                })
                .collect();
            merge_nulls(nulls, &made, asked);
            let sized = SizedText {
                bytes: arena_bytes(&places),
                places,
            };
            put(bytes, sized.bytes)?;
            handle.text = Some(sized);
            Ok(())
        })
    }
}

/// Writes the text [`tess_parquet_text_size`] checked into `arena`, of
/// `arena_len` bytes aligned to 4, each value a `text` with its header
/// (one byte for a value of up to 126 bytes, four aligned bytes
/// otherwise, little-endian), and its `Datum`, the value's address, into
/// `values`, of at least the window's held rows; the places of the other
/// rows are left as they were.
///
/// # Safety
///
/// `reader` must be live; `arena` must point to `arena_len` writable bytes
/// aligned to 4; `values` to `values_len` writable words, initialized by
/// the caller; the two may not overlap.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_parquet_text(
    reader: *mut ReaderHandle,
    arena: *mut u8,
    arena_len: usize,
    values: *mut u64,
    values_len: usize,
    status: *mut Status,
) -> bool {
    // SAFETY: as the function's contract says.
    unsafe {
        guard(status, false, || {
            let handle = reader.as_mut().ok_or_else(|| broken("a null reader"))?;
            let text = handle
                .text
                .as_ref()
                .ok_or_else(|| broken("no text sized for the window held"))?;
            if arena_len < text.bytes {
                return Err(broken("an arena shorter than its size"));
            }
            if !(arena as usize).is_multiple_of(4) || (arena.is_null() && text.bytes > 0) {
                return Err(broken("an arena not aligned to 4 bytes"));
            }
            let values = words_mut(values, values_len)?;
            if values.len() < text.places.len() {
                return Err(broken("values shorter than the window"));
            }
            let mut at = 0usize;
            for (row, place) in text.places.iter().enumerate() {
                let Some((ptr, len)) = *place else {
                    continue;
                };
                // SAFETY: the bytes of the window the handle holds, which
                // no read replaced since `tess_parquet_text_size`: a read
                // drops the sized text with the window.
                let value = std::slice::from_raw_parts(ptr, len);
                let start = if len <= SHORT_MAX {
                    // A short header: the total length, header included,
                    // shifted up a bit, the low bit set.
                    arena.add(at).write((((len + 1) << 1) | 1) as u8);
                    at + 1
                } else {
                    at = at.next_multiple_of(4);
                    // A header of four bytes: the total length shifted up
                    // two bits, little-endian.
                    let header = u32::try_from((len + 4) << 2)
                        .map_err(|_| broken("a value past the size of a header"))?;
                    std::ptr::copy_nonoverlapping(header.to_le_bytes().as_ptr(), arena.add(at), 4);
                    at + 4
                };
                std::ptr::copy_nonoverlapping(value.as_ptr(), arena.add(start), len);
                values[row] = arena.add(at) as u64;
                at = start + len;
            }
            Ok(())
        })
    }
}

/// Where a window's bytes form goes, as `TessParquetBytes`: the caller's
/// arrays of `len` entries, a row of the window each, and what the call
/// writes of the window's dictionary.
#[repr(C)]
pub struct BytesOut {
    /// A pointer a row, to the first of its bytes.
    pub data: *mut *const u8,
    /// A count of bytes a row.
    pub lengths: *mut u32,
    /// An index a row in the window's dictionary, written where the window
    /// has one; may be null for a window without one.
    pub indexes: *mut i32,
    /// The entries of each array: at least the window's rows.
    pub len: usize,
    /// The window's count of dictionary values, 0 for a plain window.
    pub nvalues: u32,
    /// The reader's number of the window's array of dictionary values, 1
    /// first and anew for each other array; 0 for a plain window.
    pub number: u64,
}

/// A reference to a window's Arrow array, as `TessParquetHold`, which keeps
/// the buffers that the pointers of [`tess_parquet_bytes`] name until it is
/// freed.
pub struct HoldHandle(#[allow(dead_code)] ArrayRef);

thread_local! {
    /// The references of this thread, a backend's in PostgreSQL, made and
    /// not yet freed: what [`tess_parquet_test_holds`] counts.
    static HOLDS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

impl HoldHandle {
    fn new(array: ArrayRef) -> Self {
        HOLDS.with(|holds| holds.set(holds.get() + 1));
        Self(array)
    }
}

impl Drop for HoldHandle {
    fn drop(&mut self) {
        HOLDS.with(|holds| holds.set(holds.get() - 1));
    }
}

/// The pointer of an empty value: never null, and never read.
static EMPTY: u8 = 0;

/// Why [`fill_rows`] refused its arguments.
#[derive(Debug, PartialEq, Eq)]
enum FillError {
    /// Outputs or keys shorter than the window, or keys without indexes.
    Short,
    /// A value of more bytes than a `u32` counts.
    Long,
    /// A row asked whose bytes the window does not give.
    Missing,
}

impl From<FillError> for Error {
    fn from(error: FillError) -> Self {
        broken(match error {
            FillError::Short => "outputs or keys shorter than the window",
            FillError::Long => "a value past what a u32 counts",
            FillError::Missing => "a row asked without its bytes",
        })
    }
}

/// Writes, for each row of a window of `rows` rows that `asked` asks and
/// `made` does not mark `NULL`, the place of its bytes, which `value`
/// gives, into `data` (that of [`EMPTY`] for an empty value), their count
/// into `lengths` and, where `keys` is given, the row's index into
/// `indexes`; the other rows' entries are left as they were, and on a
/// refusal every entry is. `value` must give a row the same answer each
/// time: it is asked once to check every row and again to write it.
///
/// # Errors
///
/// [`FillError`] for outputs shorter than the window, keys shorter than it,
/// a row whose bytes `value` does not give, or a value of more bytes than
/// a `u32` counts.
#[allow(clippy::too_many_arguments)]
fn fill_rows<'v>(
    rows: usize,
    value: impl Fn(usize) -> Option<&'v [u8]>,
    made: &[u64],
    asked: &[u64],
    keys: Option<&[i32]>,
    data: &mut [*const u8],
    lengths: &mut [u32],
    mut indexes: Option<&mut [i32]>,
) -> std::result::Result<(), FillError> {
    if data.len() < rows
        || lengths.len() < rows
        || keys.is_some_and(|keys| keys.len() < rows)
        || indexes.as_ref().is_some_and(|indexes| indexes.len() < rows)
        || keys.is_some() != indexes.is_some()
    {
        return Err(FillError::Short);
    }
    // Every row's bytes given and their count fitting before any is
    // written, so that a refusal writes nothing.
    each_given(rows, asked, made, |row| {
        let value = value(row).ok_or(FillError::Missing)?;
        u32::try_from(value.len()).map_err(|_| FillError::Long)?;
        Ok(())
    })?;
    each_given(rows, asked, made, |row| {
        let value = value(row).ok_or(FillError::Missing)?;
        data[row] = if value.is_empty() {
            &EMPTY
        } else {
            value.as_ptr()
        };
        lengths[row] = u32::try_from(value.len()).map_err(|_| FillError::Long)?;
        if let (Some(keys), Some(indexes)) = (keys, indexes.as_deref_mut()) {
            indexes[row] = keys[row];
        }
        Ok(())
    })
}

/// Calls `each` for each row below `rows` that `asked` asks and `made`
/// does not mark `NULL`, in order, by the set bits of their words, up to
/// the first error.
fn each_given<E>(
    rows: usize,
    asked: &[u64],
    made: &[u64],
    mut each: impl FnMut(usize) -> std::result::Result<(), E>,
) -> std::result::Result<(), E> {
    for word in 0..rows.div_ceil(64) {
        let mut bits =
            asked.get(word).copied().unwrap_or(0) & !made.get(word).copied().unwrap_or(0);
        while bits != 0 {
            let row = word * 64 + bits.trailing_zeros() as usize;
            bits &= bits - 1;
            if row >= rows {
                break;
            }
            each(row)?;
        }
    }
    Ok(())
}

/// The addresses that tell a dictionary's values array from another while
/// it is held: its offsets', its bytes' and its count.
fn values_key(values: &dyn Array) -> Result<(usize, usize, usize)> {
    let bytes = values
        .as_any()
        .downcast_ref::<arrow_array::LargeBinaryArray>()
        .ok_or_else(|| broken("a dictionary of values that are not bytes"))?;
    Ok((
        bytes.value_offsets().as_ptr() as usize,
        bytes.values().as_ptr() as usize,
        bytes.len(),
    ))
}

/// The rows `asked` asks of `window`, read row by row through the column's
/// plan, which checks each by the column's rule: their bytes, and a mask
/// of `words` words of their `NULL` rows; `buffers` takes the slices of a
/// view array's buffers that the bytes are read through.
fn read_by_rows<'a>(
    reader: &ColumnReader,
    window: &'a Window,
    asked: &[u64],
    words: usize,
    buffers: &'a mut Vec<&'a [u8]>,
) -> Result<(Vec<&'a [u8]>, Vec<u64>)> {
    let input = input_of(window.array.as_ref(), buffers)?;
    let mut values: Vec<&[u8]> = vec![&[]; window.array.len()];
    let mut made = vec![0u64; words];
    reader.plan().read(
        &input,
        asked,
        &mut ReadOutput::Bytes {
            values: &mut values,
            nulls: &mut made,
        },
    )?;
    Ok((values, made))
}

/// Where the rows `asked` asks of a window of a column of text that the
/// plan checks for UTF-8 and zero bytes and counts no characters of find
/// their bytes, by a check cheaper than the plan's row by row. Through a
/// dictionary, the first window that names its values array reads the
/// whole array once through the plan, into `dictionary_passes`: where
/// every value passes and none is `NULL`, every window over it takes its
/// rows' bytes unchecked again. Plain, the window's views pass value by
/// value ([`crate::bytes::views_pass`]), made once a window into
/// `window_passes`. `made` gets the bits of the rows asked that are `NULL`.
/// `None` where the column is of another rule, or where the dictionary or
/// the window did not pass: the caller then reads row by row, which fails
/// as the plan fails, and at the plan's row; as it does where a row's
/// offsets or index do not hold ([`Slots::value`]).
#[allow(clippy::too_many_arguments)]
fn cheaper_check<'a>(
    reader: &ColumnReader,
    checks: (&mut usize, &mut usize),
    array: &'a dyn Array,
    dictionary: Option<&'a dyn Array>,
    asked: &[u64],
    dictionary_passes: &mut Option<bool>,
    window_passes: &mut Option<bool>,
    made: &mut [u64],
) -> Result<Option<Slots<'a>>> {
    let text = Failures::NOT_UTF8 | Failures::ZERO_BYTE | Failures::TOO_LARGE;
    let rows = array.len();
    // A mask the plan refuses, shorter than the window or asking past it,
    // or a mask of NULLs shorter than the window, is the plan's to refuse.
    let words = mask_words(rows);
    let past = |mask: &[u64]| {
        mask.iter().enumerate().any(|(w, &bits)| {
            let below = rows.saturating_sub(w * 64).min(64);
            below < 64 && bits >> below != 0
        })
    };
    if reader.plan().failures() != text || asked.len() < words || made.len() < words || past(asked)
    {
        return Ok(None);
    }
    match dictionary {
        Some(values) => {
            let Some(keys) = array
                .as_any_dictionary_opt()
                .and_then(|d| d.keys().as_any().downcast_ref::<arrow_array::Int32Array>())
            else {
                return Ok(None);
            };
            let Some(bytes) = values
                .as_any()
                .downcast_ref::<arrow_array::LargeBinaryArray>()
            else {
                return Ok(None);
            };
            if dictionary_passes.is_none() {
                // The whole array once, a value not named by any row asked
                // among them: a value that fails only sends its windows to
                // the read row by row, which fails where a row asks it.
                let passes = bytes.null_count() == 0 && {
                    let all: Vec<u64> = (0..mask_words(bytes.len()))
                        .map(|w| match (bytes.len() - w * 64).min(64) {
                            64 => u64::MAX,
                            n => (1u64 << n) - 1,
                        })
                        .collect();
                    let mut sink: Vec<&[u8]> = vec![&[]; bytes.len()];
                    let mut sink_nulls = vec![0u64; all.len()];
                    reader
                        .plain_plan()
                        .read(
                            &input_of(values, &mut Vec::new())?,
                            &all,
                            &mut ReadOutput::Bytes {
                                values: &mut sink,
                                nulls: &mut sink_nulls,
                            },
                        )
                        .is_ok()
                };
                *checks.0 += bytes.len();
                *dictionary_passes = Some(passes);
            }
            if *dictionary_passes != Some(true) {
                return Ok(None);
            }
            Ok(
                mark_nulls(rows, asked, |row| keys.is_null(row), made).map(|()| {
                    Slots::Dictionary {
                        offsets: bytes.value_offsets(),
                        data: bytes.values(),
                        keys: keys.values(),
                    }
                }),
            )
        }
        None => {
            let Some(views) = array
                .as_any()
                .downcast_ref::<arrow_array::BinaryViewArray>()
            else {
                return Ok(None);
            };
            let passes = *window_passes.get_or_insert_with(|| {
                *checks.1 += 1;
                crate::bytes::views_pass(views)
            });
            if !passes {
                return Ok(None);
            }
            Ok(mark_nulls(rows, asked, |row| views.is_null(row), made)
                .map(|()| Slots::Views(views)))
        }
    }
}

/// Gives the rows `asked` asks of the last window in the bytes form, as
/// the column's read plan checks them (`22021`, `22001`, `54000` at the
/// first row that fails, and no row not asked): into `out`'s arrays, for
/// each row not `NULL`, the place of its bytes in the window's buffers
/// and their count and, for a window read through a dictionary, its
/// index; the rows' bits of `NULL` set or cleared in `nulls` as
/// [`tess_parquet_values`] does; and the window's count of dictionary
/// values and the reader's number of its values array, 0 and 0 for a
/// plain window. Into `hold`, a new reference to the window's array that
/// keeps those bytes where they are, whatever the reader reads after,
/// until [`tess_parquet_hold_free`] frees it. On a failure no output is
/// written and no reference is made.
///
/// # Safety
///
/// `reader` must be live; `asked` must point to `asked_words` readable
/// words and `nulls` to `nulls_words` writable ones, initialized by the
/// caller and not overlapping; `out` must point to a `BytesOut` whose
/// arrays and `len` the caller set, and whose arrays point to `len`
/// writable entries each that the caller initialized, `indexes` too
/// unless it is null; its `nvalues` and `number` may be uninitialized;
/// `hold` must be writable; none of `asked`, `nulls`, `out` and its three
/// arrays may overlap another.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_parquet_bytes(
    reader: *mut ReaderHandle,
    asked: *const u64,
    asked_words: usize,
    nulls: *mut u64,
    nulls_words: usize,
    out: *mut BytesOut,
    hold: *mut *mut HoldHandle,
    status: *mut Status,
) -> bool {
    // SAFETY: as the function's contract says.
    unsafe {
        guard(status, false, || {
            let handle = reader.as_mut().ok_or_else(|| broken("a null reader"))?;
            let asked = words(asked, asked_words)?;
            let nulls = words_mut(nulls, nulls_words)?;
            if out.is_null() || hold.is_null() {
                return Err(broken("a null output"));
            }
            // The fields the caller set, read without a reference to the
            // structure, whose outputs may be uninitialized.
            let out_data = std::ptr::addr_of!((*out).data).read();
            let out_lengths = std::ptr::addr_of!((*out).lengths).read();
            let out_indexes = std::ptr::addr_of!((*out).indexes).read();
            let out_len = std::ptr::addr_of!((*out).len).read();
            if out_data.is_null() || out_lengths.is_null() {
                return Err(broken("a null output"));
            }
            let window = handle
                .window
                .as_ref()
                .ok_or_else(|| broken("no window read"))?;
            let len = window.array.len();
            if out_len < len {
                return Err(broken("outputs shorter than the window"));
            }
            let dictionary = window
                .array
                .as_any_dictionary_opt()
                .map(|d| (d.keys(), Arc::clone(d.values())));
            let keys = match &dictionary {
                Some((keys, _)) => Some(
                    keys.as_any()
                        .downcast_ref::<arrow_array::Int32Array>()
                        .ok_or_else(|| broken("a dictionary of indexes that are not int32"))?
                        .values()
                        .as_ref(),
                ),
                None => None,
            };
            if keys.is_some() && out_indexes.is_null() {
                return Err(broken("a null output of indexes for a dictionary"));
            }
            // The dictionary's count and number, decided before any output
            // is written: a new number for an array other than the one
            // numbered last.
            let numbering = match &dictionary {
                Some((_, dvalues)) => {
                    let nvalues = u32::try_from(dvalues.len())
                        .map_err(|_| broken("a dictionary past what a u32 counts"))?;
                    let key = values_key(dvalues.as_ref())?;
                    let held = handle.numbered.as_ref().and_then(|(held, number)| {
                        (values_key(held.as_ref()).ok() == Some(key)).then_some(*number)
                    });
                    let number = match held {
                        Some(number) => number,
                        None => handle
                            .numbers
                            .checked_add(1)
                            .ok_or_else(|| broken("no number of a dictionary is left"))?,
                    };
                    Some((nvalues, number, held.is_none().then(|| Arc::clone(dvalues))))
                }
                None => None,
            };
            // What is known of this window's values array: the held array's,
            // or nothing of a new one.
            let mut dictionary_passes = match &numbering {
                Some((_, _, None)) => handle.dictionary_passes,
                _ => None,
            };
            // The mask of the rows asked that are NULL, the reader's own,
            // so that an ask allocates none.
            let mut made = std::mem::take(&mut handle.made);
            made.clear();
            made.resize(nulls.len(), 0);
            #[cfg(test)]
            let (rows_only, max) = (handle.rows_only, handle.max_fast);
            #[cfg(not(test))]
            let (rows_only, max) = (false, MAX_VALUE_BYTES);
            let fast = if rows_only {
                None
            } else {
                cheaper_check(
                    &handle.reader,
                    (&mut handle.values_checked, &mut handle.windows_checked),
                    window.array.as_ref(),
                    dictionary.as_ref().map(|(_, v)| v.as_ref()),
                    asked,
                    &mut dictionary_passes,
                    &mut handle.window_passes,
                    &mut made,
                )?
            };
            if cfg!(debug_assertions)
                && let Some(slots) = &fast
            {
                // The cheaper check agrees with the plan's, row by row,
                // before any output is written: every row asked passes it,
                // at the same bytes; a row the plan fails is the check's
                // fault, not the row's, and so an error of its own. Rows
                // it cannot give go to the read row by row below.
                let bit = |mask: &[u64], row: usize| mask[row / 64] >> (row % 64) & 1 == 1;
                let fast_values: Option<Vec<&[u8]>> = (0..len)
                    .map(|row| match bit(asked, row) && !bit(&made, row) {
                        true => slots.value(row, max),
                        false => Some(&[][..]),
                    })
                    .collect();
                if let Some(fast_values) = fast_values {
                    let mut buffers = Vec::new();
                    match read_by_rows(&handle.reader, window, asked, nulls.len(), &mut buffers) {
                        Ok((by_rows, rows_made)) if by_rows == fast_values && rows_made == made => {
                        }
                        _ => {
                            return Err(broken(
                                "the cheaper check of a window differs from the plan's",
                            ));
                        }
                    }
                }
            }
            // SAFETY: the caller gives `len` initialized, writable entries
            // at each.
            let data = std::slice::from_raw_parts_mut(out_data, len);
            let lengths = std::slice::from_raw_parts_mut(out_lengths, len);
            let mut indexes = keys
                .is_some()
                .then(|| std::slice::from_raw_parts_mut(out_indexes, len));
            // The cheaper check's rows written in place; a row it cannot
            // give, as a refusal that writes nothing, sends the window to
            // the read row by row.
            let filled = match &fast {
                Some(slots) => match fill_rows(
                    len,
                    |row| slots.value(row, max),
                    &made,
                    asked,
                    keys,
                    data,
                    lengths,
                    indexes.as_deref_mut(),
                ) {
                    Err(FillError::Missing) => false,
                    filled => {
                        filled?;
                        true
                    }
                },
                None => false,
            };
            if !filled {
                let mut buffers = Vec::new();
                let (values, rows_made) =
                    read_by_rows(&handle.reader, window, asked, nulls.len(), &mut buffers)?;
                made = rows_made;
                fill_rows(
                    len,
                    |row| values.get(row).copied(),
                    &made,
                    asked,
                    keys,
                    data,
                    lengths,
                    indexes,
                )?;
            }
            merge_nulls(nulls, &made, asked);
            handle.made = made;
            let (nvalues, number) = match numbering {
                Some((nvalues, number, new)) => {
                    if let Some(values) = new {
                        handle.numbers = number;
                        handle.numbered = Some((values, number));
                    }
                    handle.dictionary_passes = dictionary_passes;
                    (nvalues, number)
                }
                None => (0, 0),
            };
            std::ptr::addr_of_mut!((*out).nvalues).write(nvalues);
            std::ptr::addr_of_mut!((*out).number).write(number);
            put(
                hold,
                Box::into_raw(Box::new(HoldHandle::new(Arc::clone(&window.array)))),
            )
        })
    }
}

/// Frees a reference of [`tess_parquet_bytes`]; the pointers it kept
/// valid may be dangling after.
///
/// # Safety
///
/// `hold` must be null or a reference of [`tess_parquet_bytes`] not yet
/// freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_parquet_hold_free(hold: *mut HoldHandle) {
    if !hold.is_null() {
        // SAFETY: a reference of `tess_parquet_bytes`, freed once.
        drop(unsafe { Box::from_raw(hold) });
    }
}

/// The references of [`tess_parquet_bytes`] that the calling thread made
/// and has not freed, for the tests of their lifetime: a count of the
/// thread, since a PostgreSQL backend is one, so that tests on other
/// threads do not move it.
#[unsafe(no_mangle)]
pub extern "C" fn tess_parquet_test_holds() -> u64 {
    HOLDS.with(std::cell::Cell::get)
}

/// Raises a panic inside the guard, for the test of the boundary.
///
/// # Safety
///
/// `status` as [`guard`] says.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_parquet_test_panic(status: *mut Status) -> bool {
    // SAFETY: as the function's contract says.
    unsafe {
        guard(status, false, || {
            #[allow(clippy::panic)]
            {
                panic!("a panic on purpose");
            }
        })
    }
}

#[cfg(test)]
mod tests;

#[cfg(kani)]
mod proofs;
