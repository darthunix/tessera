//! A column of a group of rows read window by window for a selection of
//! its rows, and the Arrow arrays it gives as the input of `datum-arrow`.
//!
//! A column asked window after window keeps one reader of the `parquet`
//! crate that goes forward over the group with the group's selection, so
//! that each page is decompressed and the dictionary decoded once a group;
//! its arrays hold a fixed count of selected rows, so it may read into the
//! next window, and it keeps what it read past the window for the next
//! ask. A window after a gap, past windows no one asked for, builds the
//! forward reader anew from the window, which skips the pages of the gap.
//! An ask behind it builds a reader of its own for the rows asked, and
//! leaves the forward one as it was.

use std::fs::File;
use std::io::{self, BufReader, Read};
use std::os::unix::fs::FileExt;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use arrow_array::cast::AsArray;
use arrow_array::types::{
    Date32Type, Float32Type, Float64Type, Int8Type, Int16Type, Int32Type, Int64Type,
    TimestampMicrosecondType, TimestampMillisecondType, UInt8Type, UInt16Type, UInt32Type,
};
use arrow_array::{Array, ArrayRef};
use arrow_schema::{DataType, TimeUnit};
use bytes::Bytes;
use datum_arrow::{Bitmap, Column, Input, Keys, ReadPlan, Values, mask_words};
use parquet::arrow::arrow_reader::{ParquetRecordBatchReader, RowSelection, RowSelector};
use parquet::errors::ParquetError;
use parquet::file::reader::{ChunkReader, Length};

use crate::column::MappedColumn;
use crate::error::{DATA_CORRUPTED, Error, INTERNAL_ERROR, Result};
use crate::file::ParquetFile;

/// The open file shared by every reader of it: reads at an offset, so
/// that no reader opens or duplicates a descriptor.
///
/// It keeps the last error of the system's reads, other than a read past
/// the end, since the `parquet` crate passes a read's error on to Arrow as
/// text: the reader then reports `58030`, not bytes it cannot decode. It
/// counts the reads that reached the file, so that a reader checks the
/// file after the windows whose pages it read, not after those it took
/// from pages read before. A copy made by [`SharedFile::capturing`] also
/// keeps the first [`KEPT`] bytes read through each of its readers from
/// an offset, which the `parquet` crate's page reader reads a page's
/// header by and nothing else: the reader's own page reader checks a
/// page's size by them.
#[derive(Clone, Debug)]
pub(crate) struct SharedFile {
    file: Arc<File>,
    len: u64,
    failed: Arc<Mutex<Option<(io::ErrorKind, String)>>>,
    reads: Arc<AtomicU64>,
    captured: Option<Arc<Mutex<Vec<u8>>>>,
}

impl SharedFile {
    pub(crate) fn of(file: &ParquetFile) -> Self {
        SharedFile {
            file: Arc::clone(file.handle()),
            len: file.identity().size,
            failed: Arc::new(Mutex::new(None)),
            reads: Arc::new(AtomicU64::new(0)),
            captured: None,
        }
    }

    /// A copy of the file that keeps the first [`KEPT`] bytes each of its
    /// readers from an offset reads, until [`SharedFile::take_captured`]
    /// takes them.
    pub(crate) fn capturing(&self) -> Self {
        SharedFile {
            captured: Some(Arc::new(Mutex::new(Vec::new()))),
            ..self.clone()
        }
    }

    /// The bytes kept from this copy's readers from an offset since the
    /// last call; none for a copy that does not keep them.
    pub(crate) fn take_captured(&self) -> Vec<u8> {
        self.captured
            .as_ref()
            .and_then(|c| c.lock().ok().map(|mut c| std::mem::take(&mut *c)))
            .unwrap_or_default()
    }

    /// How many reads reached the file.
    pub(crate) fn reads(&self) -> u64 {
        self.reads.load(Ordering::Relaxed)
    }

    fn note(failed: &Mutex<Option<(io::ErrorKind, String)>>, error: &io::Error) {
        // A read past the end is the format's, an interrupted one is
        // retried by the crate's readers: neither is a failure of the
        // system.
        if !matches!(
            error.kind(),
            io::ErrorKind::UnexpectedEof | io::ErrorKind::Interrupted
        ) && let Ok(mut slot) = failed.lock()
        {
            *slot = Some((error.kind(), error.to_string()));
        }
    }

    /// The error of the system's reads since the last call, as `58030`.
    pub(crate) fn take_failure(&self) -> Option<Error> {
        let (kind, text) = self.failed.lock().ok()?.take()?;
        Some(crate::error::io(
            &io::Error::new(kind, text),
            "read Parquet file",
        ))
    }
}

impl Length for SharedFile {
    fn len(&self) -> u64 {
        self.len
    }
}

/// The bytes of a page's header a capturing copy keeps: the most the
/// page reader parses of its first fields.
pub(crate) const KEPT: u64 = 12;

/// The buffer of a reader from an offset: one read of the file for a
/// header of up to a kilobyte, which the crate's Thrift reader reads
/// mostly a byte at a time, and little of the page's data after it,
/// which the crate reads again by `get_bytes`.
const BUFFER: usize = 1024;

/// A reader of the shared file from an offset forward.
pub(crate) struct At {
    file: Arc<File>,
    start: u64,
    position: u64,
    failed: Arc<Mutex<Option<(io::ErrorKind, String)>>>,
    reads: Arc<AtomicU64>,
    captured: Option<Arc<Mutex<Vec<u8>>>>,
}

impl Read for At {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        let read = self
            .file
            .read_at(buf, self.position)
            .inspect_err(|e| SharedFile::note(&self.failed, e))?;
        let kept = (self.start + KEPT).saturating_sub(self.position);
        self.position += read as u64;
        if kept > 0
            && let Some(captured) = &self.captured
            && let Ok(mut captured) = captured.lock()
        {
            captured.extend_from_slice(&buf[..read.min(kept as usize)]);
        }
        Ok(read)
    }
}

impl ChunkReader for SharedFile {
    type T = BufReader<At>;

    /// A buffered reader from `start`: it gives each read no more bytes
    /// than it asks, so the crate's count of a header's bytes holds.
    fn get_read(&self, start: u64) -> parquet::errors::Result<BufReader<At>> {
        Ok(BufReader::with_capacity(
            BUFFER,
            At {
                file: Arc::clone(&self.file),
                start,
                position: start,
                failed: Arc::clone(&self.failed),
                reads: Arc::clone(&self.reads),
                captured: self.captured.clone(),
            },
        ))
    }

    fn get_bytes(&self, start: u64, length: usize) -> parquet::errors::Result<Bytes> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        let mut bytes = vec![0u8; length];
        self.file.read_exact_at(&mut bytes, start).map_err(|e| {
            SharedFile::note(&self.failed, &e);
            ParquetError::External(Box::new(e))
        })?;
        Ok(Bytes::from(bytes))
    }
}

/// The rows of a window a read gave: their values, one Arrow array of the
/// rows in order, and the mask over the window's rows of the rows it holds.
#[derive(Clone, Debug)]
pub struct Window {
    /// The values of the rows the mask holds, in their order.
    pub array: ArrayRef,
    /// A bit for each row of the window, set for the rows the array holds.
    pub rows: Vec<u64>,
}

struct Forward {
    reader: ParquetRecordBatchReader,
    /// The group's row at which the next forward window starts.
    next: usize,
    /// Selected rows read past `next`, in their order.
    pending: Vec<ArrayRef>,
}

/// A column of one group of rows, read for a selection of its rows.
pub struct ColumnReader {
    file: ParquetFile,
    column: MappedColumn,
    group: usize,
    rows: usize,
    selection: Arc<[u64]>,
    batch: usize,
    dictionary: bool,
    forward: Option<Forward>,
    shared: SharedFile,
    /// The reads of the file at its last check.
    checked: u64,
}

impl ColumnReader {
    /// A reader of `column` in group `group` of `file`, for the rows
    /// `selection` holds, a bit for each row of the group; its arrays hold
    /// `batch` selected rows.
    ///
    /// # Errors
    ///
    /// `XX000` for a group past the file's, a selection of fewer words than
    /// the group's rows need, or a batch of no rows.
    pub fn new(
        file: &ParquetFile,
        column: &MappedColumn,
        group: usize,
        selection: Arc<[u64]>,
        batch: usize,
    ) -> Result<Self> {
        let rows = file
            .group_rows(group)
            .and_then(|r| usize::try_from(r).ok())
            .ok_or_else(|| broken("a group of rows past the file's"))?;
        if selection.len() < mask_words(rows) || batch == 0 {
            return Err(broken(
                "a selection shorter than its group, or a batch of no rows",
            ));
        }
        let chunk = file.metadata().row_group(group).column(column.leaf());
        Ok(ColumnReader {
            file: file.clone(),
            column: column.clone(),
            group,
            rows,
            selection,
            batch,
            // A chunk that falls back to plain pages is read plain: the
            // crate would build a dictionary of each array of its plain
            // pages, hashing every value, and a window over two arrays
            // would merge their dictionaries.
            dictionary: crate::stats::through_dictionary(chunk) != Some(false),
            forward: None,
            shared: SharedFile::of(file),
            checked: 0,
        })
    }

    /// The read plan of the arrays the reader gives: of a dictionary array
    /// for a chunk of bytes that has a dictionary page and no data page
    /// the footer says is encoded otherwise.
    pub fn plan(&self) -> &ReadPlan {
        self.column.view(self.dictionary).1
    }

    /// The read plan of a dictionary's values read as a column of their
    /// own.
    pub fn plain_plan(&self) -> &ReadPlan {
        self.column.values_plan()
    }

    /// Reads the window of `len` rows from the group's row `start`.
    ///
    /// Where the window starts where the last forward window ended or
    /// after it, or is the first asked, the forward reader gives every
    /// selected row of the window, built anew from the window after a gap;
    /// otherwise, for a window behind it, a reader of its own gives the
    /// rows `rows` asks, a mask over the window's rows that the selection
    /// holds. The file is checked unchanged after a read that reached it,
    /// before what it read is used; a window whose pages were read before
    /// is not checked again.
    ///
    /// # Errors
    ///
    /// `XX000` for a window past the group or a mask of rows shorter than
    /// the window or holding a row the selection does not; `55000` for a
    /// file that changed, whether the read failed or not; `58030` for a
    /// read the system failed; `XX001` for bytes the format cannot
    /// decode, a panic of the crate's decoders among them.
    pub fn read(&mut self, start: usize, len: usize, rows: &[u64]) -> Result<Window> {
        let end = start
            .checked_add(len)
            .filter(|&end| end <= self.rows)
            .ok_or_else(|| broken("a window past its group"))?;
        let words = mask_words(len);
        if rows.len() < words {
            return Err(broken("a mask of rows shorter than its window"));
        }
        let selected = bits(&self.selection, start, len);
        if rows[..words]
            .iter()
            .zip(&selected)
            .any(|(asked, sel)| asked & !sel != 0)
        {
            return Err(broken("a row asked that the selection does not hold"));
        }
        let forward = match &self.forward {
            None => true,
            Some(f) => f.next <= start,
        };
        // No failure of an earlier read is this one's.
        let _ = self.shared.take_failure();
        // A panic of the crate's decoders over bytes it cannot decode is an
        // error of this read, so that the check of the file below decides
        // between a change and damaged bytes.
        let read = panic::catch_unwind(AssertUnwindSafe(|| {
            if forward {
                let count = count_of(&selected);
                self.forward_read(start, end, count).map(|array| Window {
                    array,
                    rows: selected,
                })
            } else {
                let asked = rows[..words].to_vec();
                self.read_rows(start, len, &asked)
                    .map(|array| Window { array, rows: asked })
            }
        }))
        .unwrap_or_else(|payload| Err(panicked(payload.as_ref())));
        match read {
            Ok(window) => {
                let reads = self.shared.reads();
                if reads != self.checked {
                    self.file.check_unchanged()?;
                    self.checked = reads;
                }
                Ok(window)
            }
            Err(error) => {
                // The forward reader may have read past the rows it gave:
                // the next ask builds another.
                if forward {
                    self.forward = None;
                }
                let error = self.shared.take_failure().unwrap_or(error);
                Err(self.file.read_error(error))
            }
        }
    }

    fn forward_read(&mut self, start: usize, end: usize, count: usize) -> Result<ArrayRef> {
        // After a gap the reader starts again at the window: reading the
        // gap's rows to drop them would decode what no one asked for.
        if self.forward.as_ref().is_some_and(|f| f.next != start) {
            self.forward = None;
        }
        if self.forward.is_none() {
            let selection = selectors(&self.selection, start, self.rows);
            let reader = self.build(selection, self.batch)?;
            self.forward = Some(Forward {
                reader,
                next: start,
                pending: Vec::new(),
            });
        }
        let empty = self.empty()?;
        let Some(forward) = self.forward.as_mut() else {
            return Err(broken("a forward reader lost"));
        };
        let mut parts = std::mem::take(&mut forward.pending);
        let mut have: usize = parts.iter().map(|a| a.len()).sum();
        while have < count {
            let Some(next) = forward.reader.next() else {
                return Err(Error::new(
                    DATA_CORRUPTED,
                    "Parquet column has fewer rows than its group",
                ));
            };
            let batch = next.map_err(|e| decode(&e))?;
            let array = Arc::clone(batch.column(0));
            have += array.len();
            parts.push(array);
        }
        // The rows past the window lie in the last part, since the parts
        // before it held fewer rows than the window. They stay a slice of
        // that part alone, not of the joined window: a slice of an array of
        // views keeps every buffer of its array, and a join every buffer of
        // its parts, so that a slice of the join would carry each page read
        // so far into every window after it.
        let over = have - count;
        if over > 0
            && let Some(last) = parts.last_mut()
        {
            forward.pending.push(last.slice(last.len() - over, over));
            *last = last.slice(0, last.len() - over);
        }
        forward.next = end;
        concat(&parts, &empty)
    }

    fn read_rows(&self, start: usize, len: usize, asked: &[u64]) -> Result<ArrayRef> {
        let count = count_of(asked);
        let empty = self.empty()?;
        if count == 0 {
            return Ok(empty);
        }
        let mut rows = vec![0u64; mask_words(self.rows)];
        for row in 0..len {
            if asked[row / 64] >> (row % 64) & 1 == 1 {
                let at = start + row;
                rows[at / 64] |= 1 << (at % 64);
            }
        }
        let selection = selectors(&rows, 0, self.rows);
        let reader = self.build(selection, count)?;
        let parts = reader
            .map(|batch| batch.map(|b| Arc::clone(b.column(0))))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| decode(&e))?;
        let all = concat(&parts, &empty)?;
        if all.len() != count {
            return Err(Error::new(
                DATA_CORRUPTED,
                "Parquet column has fewer rows than its group",
            ));
        }
        Ok(all)
    }

    fn build(&self, selection: RowSelection, batch: usize) -> Result<ParquetRecordBatchReader> {
        let (view, _) = self.column.view(self.dictionary);
        crate::pages::rows(&self.shared, view, self.group, selection, batch)
            .map_err(|e| crate::error::damaged(&e))
    }

    fn empty(&self) -> Result<ArrayRef> {
        let (view, _) = self.column.view(self.dictionary);
        let field = view
            .schema()
            .fields()
            .first()
            .ok_or_else(|| broken("a view without its column"))?;
        Ok(arrow_array::new_empty_array(field.data_type()))
    }
}

/// The rows of a group of `total` rows that `rows` holds from the row
/// `from`, as the crate's runs of rows selected and skipped.
fn selectors(rows: &[u64], from: usize, total: usize) -> RowSelection {
    // The bits of the word `w`, those of the rows before `from` clear.
    let word = |w: usize| -> u64 {
        let bits = rows.get(w).copied().unwrap_or(0);
        match from.saturating_sub(w * 64) {
            0 => bits,
            below if below >= 64 => 0,
            below => bits & (!0u64 << below),
        }
    };
    let mut out = Vec::new();
    let mut row = 0;
    while row < total {
        let this = word(row / 64) >> (row % 64) & 1 == 1;
        // The run ends at the first row of the other kind, found a word at
        // a time.
        let mut at = row;
        let end = loop {
            let w = at / 64;
            let differs = if this { !word(w) } else { word(w) } & (!0u64 << (at % 64));
            if differs != 0 {
                break (w * 64 + differs.trailing_zeros() as usize).min(total);
            }
            at = (w + 1) * 64;
            if at >= total {
                break total;
            }
        };
        out.push(if this {
            RowSelector::select(end - row)
        } else {
            RowSelector::skip(end - row)
        });
        row = end;
    }
    RowSelection::from(out)
}

/// The bits of `mask` from `start` for `len` rows, as a mask of their own.
fn bits(mask: &[u64], start: usize, len: usize) -> Vec<u64> {
    let (first, shift) = (start / 64, start % 64);
    let at = |w: usize| mask.get(first + w).copied().unwrap_or(0);
    let mut out: Vec<u64> = (0..mask_words(len))
        .map(|w| match shift {
            0 => at(w),
            _ => at(w) >> shift | at(w + 1) << (64 - shift),
        })
        .collect();
    if !len.is_multiple_of(64)
        && let Some(last) = out.last_mut()
    {
        *last &= (1u64 << (len % 64)) - 1;
    }
    out
}

fn count_of(mask: &[u64]) -> usize {
    mask.iter().map(|w| w.count_ones() as usize).sum()
}

fn concat(parts: &[ArrayRef], empty: &ArrayRef) -> Result<ArrayRef> {
    match parts {
        [] => Ok(Arc::clone(empty)),
        [one] => Ok(Arc::clone(one)),
        _ => {
            let refs: Vec<&dyn Array> = parts.iter().map(|a| a.as_ref()).collect();
            arrow_select::concat::concat(&refs).map_err(|e| decode(&e))
        }
    }
}

fn decode(error: &arrow_schema::ArrowError) -> Error {
    Error::new(
        DATA_CORRUPTED,
        format!("could not decode Parquet file: {error}"),
    )
}

fn broken(message: &str) -> Error {
    Error::new(INTERNAL_ERROR, message)
}

/// The error of a panic the crate raised while it decoded a file.
pub(crate) fn panicked(payload: &(dyn std::any::Any + Send)) -> Error {
    let text = payload
        .downcast_ref::<&str>()
        .map(|t| (*t).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| String::from("a panic"));
    Error::new(
        DATA_CORRUPTED,
        format!("could not decode Parquet file: {text}"),
    )
}

/// The buffers of an Arrow array as the input of `datum-arrow`, borrowed;
/// for a view array, `buffers` takes the slices of its buffers, which the
/// input borrows, so that it lives as long as the input does.
///
/// # Errors
///
/// `XX000` for an array of a type the reader does not give.
pub fn input_of<'a>(array: &'a dyn Array, buffers: &'a mut Vec<&'a [u8]>) -> Result<Input<'a>> {
    let len = array.len();
    let nulls = nulls_of(array);
    let plain = |values| Input::Plain(Column { len, values, nulls });
    Ok(match array.data_type() {
        DataType::Dictionary(key, _) if **key == DataType::Int32 => {
            let dictionary = array.as_dictionary::<Int32Type>();
            let values = dictionary.values();
            let Input::Plain(values) = input_of(values.as_ref(), buffers)? else {
                return Err(broken("a dictionary of a dictionary"));
            };
            Input::Dictionary {
                len,
                keys: Keys::Int32(dictionary.keys().values()),
                nulls,
                values,
            }
        }
        DataType::Boolean => {
            let values = array.as_boolean().values();
            plain(Values::Boolean(Bitmap::new(
                values.inner().as_slice(),
                values.offset(),
            )))
        }
        DataType::Int8 => plain(Values::Int8(array.as_primitive::<Int8Type>().values())),
        DataType::Int16 => plain(Values::Int16(array.as_primitive::<Int16Type>().values())),
        DataType::Int32 => plain(Values::Int32(array.as_primitive::<Int32Type>().values())),
        DataType::Int64 => plain(Values::Int64(array.as_primitive::<Int64Type>().values())),
        DataType::UInt8 => plain(Values::UInt8(array.as_primitive::<UInt8Type>().values())),
        DataType::UInt16 => plain(Values::UInt16(array.as_primitive::<UInt16Type>().values())),
        DataType::UInt32 => plain(Values::UInt32(array.as_primitive::<UInt32Type>().values())),
        DataType::Float32 => plain(Values::Float32(
            array.as_primitive::<Float32Type>().values(),
        )),
        DataType::Float64 => plain(Values::Float64(
            array.as_primitive::<Float64Type>().values(),
        )),
        DataType::Date32 => plain(Values::Int32(array.as_primitive::<Date32Type>().values())),
        DataType::Timestamp(TimeUnit::Millisecond, _) => plain(Values::Int64(
            array.as_primitive::<TimestampMillisecondType>().values(),
        )),
        DataType::Timestamp(TimeUnit::Microsecond, _) => plain(Values::Int64(
            array.as_primitive::<TimestampMicrosecondType>().values(),
        )),
        DataType::LargeBinary => {
            let bytes = array.as_binary::<i64>();
            plain(Values::LargeBytes {
                offsets: bytes.value_offsets(),
                data: bytes.values().as_slice(),
            })
        }
        DataType::BinaryView => {
            let views = array.as_binary_view();
            buffers.clear();
            buffers.extend(views.data_buffers().iter().map(|b| b.as_slice()));
            plain(Values::Views {
                views: views.views().inner().as_slice(),
                buffers,
            })
        }
        DataType::FixedSizeBinary(size) => {
            let fixed = array.as_fixed_size_binary();
            let size = usize::try_from(*size).map_err(|_| broken("a negative size"))?;
            // A slice of fixed-size bytes slices its buffer of values.
            plain(Values::FixedBytes {
                size,
                data: fixed.value_data(),
            })
        }
        _ => return Err(broken("an array of a type the reader does not give")),
    })
}

fn nulls_of(array: &dyn Array) -> Option<Bitmap<'_>> {
    array
        .nulls()
        .map(|n| Bitmap::new(n.buffer().as_slice(), n.offset()))
}

#[cfg(test)]
mod tests;
