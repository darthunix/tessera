//! The pages of a column chunk as the reader hands them to the `parquet`
//! crate, so that the reader decompresses a Snappy page itself.
//!
//! The crate's builder of readers makes the decompression of a page
//! from the chunk's compression and takes no other. Its lower entry,
//! [`ParquetRecordBatchReader::try_new_with_row_groups`], reads the pages
//! a caller's [`RowGroups`] gives: here one group of rows, and for each
//! chunk the crate's page reader, over the chunk's metadata marked
//! uncompressed when the chunk is compressed with Snappy, so that it
//! hands the pages over as the file stores them. [`Snappy`] decompresses
//! them by [`crate::snappy`].

use std::sync::Arc;

use bytes::Bytes;
use parquet::arrow::arrow_reader::{
    ArrowReaderMetadata, ParquetRecordBatchReader, RowGroups, RowSelection, RowSelector,
};
use parquet::arrow::{ProjectionMask, parquet_to_arrow_field_levels};
use parquet::basic::Compression;
use parquet::column::page::{Page, PageIterator, PageMetadata, PageReader};
use parquet::errors::{ParquetError, Result};
use parquet::file::metadata::{ColumnChunkMetaData, ParquetMetaData, RowGroupMetaData};
use parquet::file::serialized_reader::SerializedPageReader;

use crate::reader::SharedFile;
use crate::snappy;

/// A reader of the rows `selection` holds of group `group` of `view`, a
/// file's metadata cut to the column read, in batches of `batch` rows.
///
/// The crate reads the selection by skipping and reading its runs, a
/// whole page skipped without decompressing it where a skip covers it.
pub(crate) fn rows(
    file: &SharedFile,
    view: &ArrowReaderMetadata,
    group: usize,
    selection: RowSelection,
    batch: usize,
) -> Result<ParquetRecordBatchReader> {
    let levels = parquet_to_arrow_field_levels(
        view.parquet_schema(),
        ProjectionMask::all(),
        Some(view.schema().fields()),
    )?;
    // As the crate's builder does, so as not to allocate past the file.
    let batch = batch.min(view.metadata().file_metadata().num_rows() as usize);
    let group = Group {
        file: file.clone(),
        metadata: Arc::clone(view.metadata()),
        group,
    };
    // The crate's default policy reads a selection whose runs are short
    // on average by a mask, which decodes every row of a batch's span, a
    // long skip inside it too. A trailing skip, which the policy counts
    // when it chooses and the plan trims before it reads, makes it choose
    // selectors, which skip whole pages; a test sees it skip them.
    let mut runs: Vec<RowSelector> = selection.into();
    runs.push(RowSelector::skip(32 * (runs.len() + 1)));
    ParquetRecordBatchReader::try_new_with_row_groups(
        &levels,
        &group,
        batch,
        Some(RowSelection::from(runs)),
    )
}

/// A reader of the pages of `chunk`, a chunk of a group of `rows` rows.
pub(crate) fn open(
    file: &SharedFile,
    chunk: &ColumnChunkMetaData,
    rows: usize,
) -> Result<Box<dyn PageReader>> {
    let reader = Arc::new(file.clone());
    if chunk.compression() != Compression::SNAPPY {
        return Ok(Box::new(SerializedPageReader::new(
            reader, chunk, rows, None,
        )?));
    }
    let stored = chunk
        .clone()
        .into_builder()
        .set_compression(Compression::UNCOMPRESSED)
        .build()?;
    // A copy of the file of its own, which keeps the first bytes of each
    // page's header as the crate's page reader reads them.
    let file = file.capturing();
    Ok(Box::new(Snappy {
        pages: SerializedPageReader::new(Arc::new(file.clone()), &stored, rows, None)?,
        file,
    }))
}

/// One group of rows of a file's metadata.
struct Group {
    file: SharedFile,
    metadata: Arc<ParquetMetaData>,
    group: usize,
}

impl RowGroups for Group {
    fn num_rows(&self) -> usize {
        usize::try_from(self.metadata.row_group(self.group).num_rows()).unwrap_or(0)
    }

    fn column_chunks(&self, i: usize) -> Result<Box<dyn PageIterator>> {
        let group = self.metadata.row_group(self.group);
        let Some(chunk) = group.columns().get(i) else {
            return Err(ParquetError::General(format!("no column {i} in the group")));
        };
        Ok(Box::new(Once(Some(open(
            &self.file,
            chunk,
            self.num_rows(),
        )?))))
    }

    fn row_groups(&self) -> Box<dyn Iterator<Item = &RowGroupMetaData> + '_> {
        Box::new(std::iter::once(self.metadata.row_group(self.group)))
    }

    fn metadata(&self) -> &ParquetMetaData {
        &self.metadata
    }
}

/// The page readers of a column's chunks in one group: one.
struct Once(Option<Box<dyn PageReader>>);

impl Iterator for Once {
    type Item = Result<Box<dyn PageReader>>;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.take().map(Ok)
    }
}

impl PageIterator for Once {}

/// The pages of a Snappy chunk, as the file stores them, decompressed;
/// `file` keeps the first bytes of the header of the page the crate's
/// page reader read last.
struct Snappy {
    pages: SerializedPageReader<SharedFile>,
    file: SharedFile,
}

impl Snappy {
    /// `page` decompressed, to the size its header gives, `size`, where the
    /// header's bytes give it.
    fn decompress(page: Page, size: Option<usize>) -> Result<Page> {
        Ok(match page {
            Page::DictionaryPage {
                buf,
                num_values,
                encoding,
                is_sorted,
            } => Page::DictionaryPage {
                buf: whole(&buf, 0, size)?,
                num_values,
                encoding,
                is_sorted,
            },
            Page::DataPage {
                buf,
                num_values,
                encoding,
                def_level_encoding,
                rep_level_encoding,
                statistics,
            } => Page::DataPage {
                buf: whole(&buf, 0, size)?,
                num_values,
                encoding,
                def_level_encoding,
                rep_level_encoding,
                statistics,
            },
            // The levels of a page of the format's second version precede
            // its values uncompressed, and only the values are compressed,
            // unless the page says they are not.
            Page::DataPageV2 {
                buf,
                num_values,
                encoding,
                num_nulls,
                num_rows,
                def_levels_byte_len,
                rep_levels_byte_len,
                is_compressed,
                statistics,
            } => Page::DataPageV2 {
                buf: if is_compressed {
                    let levels = def_levels_byte_len as usize + rep_levels_byte_len as usize;
                    whole(&buf, levels, size)?
                } else {
                    buf
                },
                num_values,
                encoding,
                num_nulls,
                num_rows,
                def_levels_byte_len,
                rep_levels_byte_len,
                is_compressed: false,
                statistics,
            },
        })
    }
}

/// `page`'s first `levels` bytes as they are, then the rest decompressed,
/// as the crate's codec decompresses it: to `size` bytes in all where the
/// page's header gives them, and an error for a stream of another length;
/// a page whose size leaves its values none, or whose rest is empty, as
/// it is.
fn whole(page: &Bytes, levels: usize, size: Option<usize>) -> Result<Bytes> {
    let Some(stream) = page.get(levels..) else {
        return Err(ParquetError::General(
            "a page shorter than its levels".into(),
        ));
    };
    if size.map_or(stream.is_empty(), |size| size == levels) {
        return Ok(page.slice(..levels));
    }
    let damaged = |e: snappy::Damaged| ParquetError::External(Box::new(e));
    let len = snappy::decompress_len(stream).map_err(damaged)?;
    if size.is_some_and(|size| size != levels + len) {
        return Err(ParquetError::General(format!(
            "a page of {} bytes decompressed where its header says {}",
            levels + len,
            size.unwrap_or(0)
        )));
    }
    #[cfg(test)]
    tests::DECOMPRESSED.with(|n| n.set(n.get() + 1));
    let mut out = vec![0; levels + len];
    out[..levels].copy_from_slice(&page[..levels]);
    snappy::decompress(stream, &mut out[levels..]).map_err(damaged)?;
    Ok(Bytes::from(out))
}

/// The decompressed size a page's header gives, `uncompressed_page_size`,
/// from the header's bytes in Thrift's compact form: its field 2, an
/// `i32`, which writers write after field 1, the page's type. `None`
/// where the header starts otherwise, which the crate's reader reads all
/// the same: the size is then not checked.
fn header_size(header: &[u8]) -> Option<usize> {
    let mut at = 0;
    let mut field = 0;
    loop {
        let byte = *header.get(at)?;
        let delta = byte >> 4;
        // A field of type i32, 5, whose id is its delta in the high nibble.
        if byte & 0x0f != 5 || delta == 0 {
            return None;
        }
        field += delta;
        if field > 2 {
            return None;
        }
        let (value, len) = varint(&header[at + 1..])?;
        at += 1 + len;
        if field == 2 {
            // Zigzag: an odd value is a negative size, which no page has.
            return if value & 1 == 0 {
                usize::try_from(value >> 1).ok()
            } else {
                None
            };
        }
    }
}

/// A varint of up to 5 bytes, an `i32`'s, and its length.
fn varint(bytes: &[u8]) -> Option<(u64, usize)> {
    let mut value = 0;
    for (i, &b) in bytes.iter().take(5).enumerate() {
        value |= u64::from(b & 0x7f) << (7 * i);
        if b < 0x80 {
            return Some((value, i + 1));
        }
    }
    None
}

impl Iterator for Snappy {
    type Item = Result<Page>;

    fn next(&mut self) -> Option<Self::Item> {
        self.get_next_page().transpose()
    }
}

impl PageReader for Snappy {
    fn get_next_page(&mut self) -> Result<Option<Page>> {
        let page = self.pages.get_next_page()?;
        // The header the page reader read, now or at a peek before.
        let size = header_size(&self.file.take_captured());
        page.map(|page| Snappy::decompress(page, size)).transpose()
    }

    fn peek_next_page(&mut self) -> Result<Option<PageMetadata>> {
        self.pages.peek_next_page()
    }

    fn skip_next_page(&mut self) -> Result<()> {
        self.pages.skip_next_page()?;
        self.file.take_captured();
        Ok(())
    }

    // `at_record_boundary` is the trait's own, which peeks at the next
    // page, as the crate's page reader answers without a page index.
}

#[cfg(test)]
mod tests;
