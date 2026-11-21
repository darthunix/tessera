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

use datum_arrow::{ReadOutput, TextBytes, Unit, mask_words};

use crate::column::{ColumnRequest, MappedColumn, map_column};
use crate::error::{DATA_CORRUPTED, Error, INTERNAL_ERROR, Result};
use crate::file::{Identity, ParquetFile};
use crate::reader::{ColumnReader, Window, input_of};

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
    /// The places of the text the last call of `tess_parquet_text_size`
    /// made, within the last window's buffers.
    text: Vec<Option<(*const u8, usize)>>,
    text_bytes: usize,
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
    // SAFETY: the caller gives null or a writable status.
    if let Some(status) = unsafe { status.as_mut() } {
        fill(status, &error);
    }
    false
}

fn fill(status: &mut Status, error: &Error) {
    status.sqlstate = [0; 6];
    for (place, byte) in status.sqlstate.iter_mut().zip(error.sqlstate().bytes()) {
        *place = byte as c_char;
    }
    status.row = error
        .row()
        .and_then(|r| i64::try_from(r).ok())
        .unwrap_or(-1);
    status.message = [0; MESSAGE_SIZE];
    let bytes = error.message().as_bytes();
    let len = bytes.len().min(MESSAGE_SIZE - 1);
    for (place, &byte) in status.message.iter_mut().zip(&bytes[..len]) {
        *place = byte as c_char;
    }
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

/// Opens the file at the path of `path_len` bytes and reads its footer;
/// where `expected` is not null, checks the file against it.
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
            let file = match expected.as_ref() {
                Some(identity) => ParquetFile::open_expecting(path, &(*identity).into())?,
                None => ParquetFile::open(path)?,
            };
            let out = out.as_mut().ok_or_else(|| broken("a null output"))?;
            *out = Box::into_raw(Box::new(FileHandle(file)));
            Ok(())
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
            let (Some(identity), Some(groups), Some(rows)) =
                (identity.as_mut(), groups.as_mut(), rows.as_mut())
            else {
                return Err(broken("a null output"));
            };
            *identity = file.identity().into();
            *groups = file.groups();
            *rows = file.rows();
            Ok(())
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
            *rows.as_mut().ok_or_else(|| broken("a null output"))? = count;
            Ok(())
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
            let column = map_column(file, &request)?;
            let out = out.as_mut().ok_or_else(|| broken("a null output"))?;
            *out = Box::into_raw(Box::new(ColumnHandle(column)));
            Ok(())
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
            let reader = ColumnReader::new(file, column, group, selection, batch)?;
            let out = out.as_mut().ok_or_else(|| broken("a null output"))?;
            *out = Box::into_raw(Box::new(ReaderHandle {
                reader,
                window: None,
                text: Vec::new(),
                text_bytes: 0,
            }));
            Ok(())
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
/// `reader` must be live; `rows` must point to `rows_words` readable words
/// and `held` to `held_words` writable ones; `count` must be writable.
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
            if held.len() < mask_words(len) {
                return Err(broken("a mask of rows held shorter than the window"));
            }
            let count = count.as_mut().ok_or_else(|| broken("a null output"))?;
            handle.window = None;
            handle.text.clear();
            let window = handle.reader.read(start, len, rows)?;
            held[..window.rows.len()].copy_from_slice(&window.rows);
            *count = window.array.len();
            handle.window = Some(window);
            Ok(())
        })
    }
}

/// Converts the rows of the last window that `asked`, a mask over its rows
/// held in their order, asks for, into `Datum` words: `values` of at least
/// the window's held rows and `nulls`, a mask of as many, as
/// `ReadPlan::read` of `datum-arrow` says for a type passed by value.
///
/// # Safety
///
/// `reader` must be live; the masks and `values` must point to as many
/// words as their lengths say, `values` and `nulls` writable.
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
            let input = input_of(window.array.as_ref())?;
            let output = &mut ReadOutput::Datums {
                values: words_mut(values, values_len)?,
                nulls: words_mut(nulls, nulls_words)?,
            };
            handle
                .reader
                .plan()
                .read(&input, words(asked, asked_words)?, output)?;
            Ok(())
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
/// column's read plan says, and writes the bytes of the arena their
/// values need into `bytes`, for [`tess_parquet_text`].
///
/// # Safety
///
/// `reader` must be live; `asked` must point to `asked_words` words and
/// `nulls` to `nulls_words` writable ones; `bytes` must be writable.
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
            let asked = words(asked, asked_words)?;
            let nulls = words_mut(nulls, nulls_words)?;
            let bytes = bytes.as_mut().ok_or_else(|| broken("a null output"))?;
            handle.text.clear();
            let window = handle
                .window
                .as_ref()
                .ok_or_else(|| broken("no window read"))?;
            let len = window.array.len();
            let input = input_of(window.array.as_ref())?;
            let mut values: Vec<&[u8]> = vec![&[]; len];
            handle.reader.plan().read(
                &input,
                asked,
                &mut ReadOutput::Bytes {
                    values: &mut values,
                    nulls,
                },
            )?;
            let places: Vec<Option<(*const u8, usize)>> = (0..len)
                .map(|row| {
                    let wanted = asked[row / 64] >> (row % 64) & 1 == 1
                        && nulls[row / 64] >> (row % 64) & 1 == 0;
                    wanted.then(|| (values[row].as_ptr(), values[row].len()))
                })
                .collect();
            handle.text_bytes = arena_bytes(&places);
            handle.text = places;
            *bytes = handle.text_bytes;
            Ok(())
        })
    }
}

/// Writes the text [`tess_parquet_text_size`] checked into `arena`, of
/// `arena_len` bytes aligned to 4, each value a `text` with its header,
/// and its `Datum`, the address of the value, into `values`, of at least
/// the window's held rows; the places of the other rows are left as they
/// were.
///
/// # Safety
///
/// `reader` must be live; `arena` must point to `arena_len` writable bytes
/// aligned to 4; `values` to `values_len` writable words.
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
            if handle.window.is_none() || arena_len < handle.text_bytes {
                return Err(broken("no text sized, or an arena shorter than its size"));
            }
            if !(arena as usize).is_multiple_of(4) || (arena.is_null() && handle.text_bytes > 0) {
                return Err(broken("an arena not aligned to 4 bytes"));
            }
            let values = words_mut(values, values_len)?;
            if values.len() < handle.text.len() {
                return Err(broken("values shorter than the window"));
            }
            let mut at = 0usize;
            for (row, place) in handle.text.iter().enumerate() {
                let Some((ptr, len)) = *place else {
                    continue;
                };
                // SAFETY: the bytes of the last window, alive in the handle,
                // which no read replaced since `tess_parquet_text_size`.
                let value = std::slice::from_raw_parts(ptr, len);
                let start = if len <= SHORT_MAX {
                    // A short header: the total length, header included,
                    // shifted up a bit, the low bit set.
                    *arena.add(at) = (((len + 1) << 1) | 1) as u8;
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
