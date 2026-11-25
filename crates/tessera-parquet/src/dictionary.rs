//! The dictionary page of a column's chunk, read alone, its values decoded
//! by the reader's own code: the `parquet` crate's decoders of values sit
//! behind its feature `experimental`.
//!
//! A dictionary page holds its values plain, whether its header says
//! `PLAIN`, as the crate's writer writes it, or `PLAIN_DICTIONARY`, as
//! writers of the format's first version do. A page of any other encoding
//! leaves the dictionary untried, without an error.

use bytes::Bytes;
use parquet::basic::Encoding;
use parquet::column::page::Page;

use crate::column::{FileType, MappedColumn};
use crate::error::{self, DATA_CORRUPTED, Error, Result};
use crate::file::ParquetFile;
use crate::reader::SharedFile;

/// The values of a chunk's dictionary, in the stored unit of the column's
/// read plan for integers, dates and times, and as their bytes for
/// strings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Dictionary {
    /// Integers, days since 1970, or milliseconds or microseconds since
    /// 1970.
    Integers(Vec<i64>),
    /// Strings, as their bytes.
    Bytes(DictionaryBytes),
}

/// The strings of a dictionary page, read in place: the page's buffer and
/// where each value's bytes lie in it, so that a dictionary of many values
/// costs no allocation a value. Two are equal where their values are.
#[derive(Clone, Debug)]
pub struct DictionaryBytes {
    page: Bytes,
    /// The start and the end of each value's bytes in `page`, each inside
    /// it.
    ranges: Vec<(usize, usize)>,
}

impl DictionaryBytes {
    /// The values, in the page's order.
    pub fn iter(&self) -> impl Iterator<Item = &[u8]> {
        self.ranges
            .iter()
            .map(|&(start, end)| &self.page[start..end])
    }
}

impl PartialEq for DictionaryBytes {
    fn eq(&self, other: &Self) -> bool {
        self.iter().eq(other.iter())
    }
}

impl Eq for DictionaryBytes {}

/// A dictionary of `values` laid one after another in a buffer of its own.
impl<'a> FromIterator<&'a [u8]> for DictionaryBytes {
    fn from_iter<I: IntoIterator<Item = &'a [u8]>>(values: I) -> Self {
        let mut page = Vec::new();
        let ranges = values
            .into_iter()
            .map(|value| {
                let start = page.len();
                page.extend_from_slice(value);
                (start, page.len())
            })
            .collect();
        Self {
            page: Bytes::from(page),
            ranges,
        }
    }
}

/// The dictionary of `column`'s chunk in group `group` of `file`: `None`
/// for a chunk without a dictionary page, of a type whose clauses the
/// source does not check, or whose page is of an encoding other than
/// plain values.
///
/// # Errors
///
/// `55000` for a file that changed, whether the read failed or not;
/// `58030` for a read the system failed; `XX001` for a page the format
/// cannot decode, a panic of the crate's decoders among them, or values
/// shorter than the page says.
pub fn read_dictionary(
    file: &ParquetFile,
    column: &MappedColumn,
    group: usize,
) -> Result<Option<Dictionary>> {
    let Some(meta) = file.metadata().row_groups().get(group) else {
        return Ok(None);
    };
    let chunk = meta.column(column.leaf());
    let width = match column.file_type() {
        FileType::Int { bits: 64, .. } | FileType::Timestamp { .. } => 8,
        FileType::Int { .. } | FileType::Date => 4,
        FileType::Bytes => 0,
        _ => return Ok(None),
    };
    if chunk.dictionary_page_offset().is_none() {
        return Ok(None);
    }
    let rows = usize::try_from(meta.num_rows()).unwrap_or(0);
    let shared = SharedFile::of(file);
    let page = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        crate::pages::open(&shared, chunk, rows)
            .and_then(|mut pages| pages.get_next_page())
            .map_err(|e| error::damaged(&e))
    }))
    .unwrap_or_else(|payload| Err(crate::reader::panicked(payload.as_ref())))
    .map_err(|e| file.read_error(shared.take_failure().unwrap_or(e)))?;
    file.check_unchanged()?;
    let Some(Page::DictionaryPage {
        buf,
        num_values,
        encoding,
        ..
    }) = page
    else {
        return Ok(None);
    };
    if !matches!(encoding, Encoding::PLAIN | Encoding::PLAIN_DICTIONARY) {
        return Ok(None);
    }
    let count = usize::try_from(num_values).unwrap_or(usize::MAX);
    let values = if width == 0 {
        Dictionary::Bytes(plain_bytes(&buf, count)?)
    } else {
        Dictionary::Integers(plain_integers(&buf, count, width, column.file_type())?)
    };
    Ok(Some(values))
}

/// `count` plain values of `width` bytes, little-endian, as the stored
/// integers of `file_type`.
fn plain_integers(buf: &[u8], count: usize, width: usize, file_type: FileType) -> Result<Vec<i64>> {
    let need = count.checked_mul(width).filter(|&n| n <= buf.len());
    if need.is_none() {
        return Err(short());
    }
    Ok(buf
        .chunks_exact(width)
        .take(count)
        .map(|bytes| match (width, file_type) {
            (4, _) => {
                let word = i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
                narrowed(word, file_type)
            }
            _ => {
                let mut word = [0u8; 8];
                word.copy_from_slice(bytes);
                i64::from_le_bytes(word)
            }
        })
        .collect())
}

/// A stored `INT32` as the reader gives its row: narrowed to its mark's
/// width, as the `parquet` crate narrows a value outside it.
pub(crate) fn narrowed(word: i32, file_type: FileType) -> i64 {
    match file_type {
        FileType::Int {
            bits: 8,
            signed: true,
        } => i64::from(word as i8),
        FileType::Int {
            bits: 8,
            signed: false,
        } => i64::from(word as u8),
        FileType::Int {
            bits: 16,
            signed: true,
        } => i64::from(word as i16),
        FileType::Int {
            bits: 16,
            signed: false,
        } => i64::from(word as u16),
        FileType::Int { signed: false, .. } => i64::from(word as u32),
        _ => i64::from(word),
    }
}

/// `count` plain values of `BYTE_ARRAY`: each a 4-byte little-endian
/// length and its bytes.
fn plain_bytes(buf: &Bytes, count: usize) -> Result<DictionaryBytes> {
    let mut ranges = Vec::with_capacity(count.min(buf.len() / 4));
    let mut at = 0usize;
    for _ in 0..count {
        let head = buf.get(at..at + 4).ok_or_else(short)?;
        let len = u32::from_le_bytes([head[0], head[1], head[2], head[3]]) as usize;
        at += 4;
        let end = at
            .checked_add(len)
            .filter(|&end| end <= buf.len())
            .ok_or_else(short)?;
        ranges.push((at, end));
        at = end;
    }
    Ok(DictionaryBytes {
        page: buf.clone(),
        ranges,
    })
}

fn short() -> Error {
    Error::new(
        DATA_CORRUPTED,
        "Parquet dictionary page holds fewer values than it says",
    )
}

#[cfg(test)]
mod tests;
