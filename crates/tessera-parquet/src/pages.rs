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
    ArrowReaderMetadata, ParquetRecordBatchReader, RowGroups, RowSelection,
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
/// The crate reads the selection by its default policy: a selection of
/// short runs by a mask, its unselected rows decoded and dropped.
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
    ParquetRecordBatchReader::try_new_with_row_groups(&levels, &group, batch, Some(selection))
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
    Ok(Box::new(Snappy(SerializedPageReader::new(
        reader, &stored, rows, None,
    )?)))
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

/// The pages of a Snappy chunk, as the file stores them, decompressed.
struct Snappy(SerializedPageReader<SharedFile>);

impl Snappy {
    fn decompress(page: Page) -> Result<Page> {
        Ok(match page {
            Page::DictionaryPage {
                buf,
                num_values,
                encoding,
                is_sorted,
            } => Page::DictionaryPage {
                buf: whole(&buf, 0)?,
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
                buf: whole(&buf, 0)?,
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
                    whole(&buf, levels)?
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

/// `page`'s first `levels` bytes as they are, then the rest
/// decompressed; an empty rest stays empty, as the crate's codec leaves
/// a page of no values.
fn whole(page: &Bytes, levels: usize) -> Result<Bytes> {
    let Some(stream) = page.get(levels..) else {
        return Err(ParquetError::General(
            "a page shorter than its levels".into(),
        ));
    };
    if stream.is_empty() {
        return Ok(page.clone());
    }
    let damaged = |e: snappy::Damaged| ParquetError::External(Box::new(e));
    let len = snappy::decompress_len(stream).map_err(damaged)?;
    let mut out = vec![0; levels + len];
    out[..levels].copy_from_slice(&page[..levels]);
    snappy::decompress(stream, &mut out[levels..]).map_err(damaged)?;
    Ok(Bytes::from(out))
}

impl Iterator for Snappy {
    type Item = Result<Page>;

    fn next(&mut self) -> Option<Self::Item> {
        self.get_next_page().transpose()
    }
}

impl PageReader for Snappy {
    fn get_next_page(&mut self) -> Result<Option<Page>> {
        self.0.get_next_page()?.map(Snappy::decompress).transpose()
    }

    fn peek_next_page(&mut self) -> Result<Option<PageMetadata>> {
        self.0.peek_next_page()
    }

    fn skip_next_page(&mut self) -> Result<()> {
        self.0.skip_next_page()
    }

    // `at_record_boundary` is the trait's own, which peeks at the next
    // page, as the crate's page reader answers without a page index.
}

#[cfg(test)]
mod tests;
