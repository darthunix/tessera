use std::cell::Cell;
use std::fs::File;
use std::path::{Path, PathBuf};

use arrow_array::types::Int64Type;
use arrow_array::{ArrayRef, Int64Array, ListArray, RecordBatch, StringArray};
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_reader::{
    ArrowReaderOptions, ParquetRecordBatchReaderBuilder, RowSelectionPolicy, RowSelector,
};
use parquet::file::properties::{WriterProperties, WriterVersion};

use super::*;
use crate::file::ParquetFile;

const ROWS: usize = 6_000;

thread_local! {
    /// The streams `whole` decompressed on this thread.
    pub(super) static DECOMPRESSED: Cell<usize> = const { Cell::new(0) };
}

/// A file of strings, through a dictionary that grows past its limit and
/// then plain, of integers, and of lists of integers, whose pages hold
/// levels of repetition, all with `NULL`s, in groups of 2 500 rows and
/// pages of 300, of the format's `version`.
fn file(dir: &Path, compression: Compression, version: WriterVersion) -> PathBuf {
    let urls: Vec<Option<String>> = (0..ROWS)
        .map(|i| (i % 7 != 0).then(|| format!("http://example.com/{}/{}", i % 50, i)))
        .collect();
    let ids: Vec<Option<i64>> = (0..ROWS as i64)
        .map(|i| (i % 5 != 0).then_some(i * 3))
        .collect();
    let lists = ListArray::from_iter_primitive::<Int64Type, _, _>((0..ROWS as i64).map(|i| {
        (i % 11 != 0).then(|| {
            (0..i % 4)
                .map(|j| (j != 2).then_some(i * 10 + j))
                .collect::<Vec<_>>()
        })
    }));
    let batch = RecordBatch::try_from_iter([
        ("url", Arc::new(StringArray::from(urls)) as ArrayRef),
        ("id", Arc::new(Int64Array::from(ids)) as ArrayRef),
        ("lists", Arc::new(lists) as ArrayRef),
    ])
    .unwrap();
    let properties = WriterProperties::builder()
        .set_compression(compression)
        .set_writer_version(version)
        .set_max_row_group_row_count(Some(2_500))
        .set_data_page_row_count_limit(300)
        .set_write_batch_size(300)
        .set_dictionary_page_size_limit(2_000)
        .build();
    let path = dir.join(format!("{compression:?}-{version:?}.parquet"));
    let mut writer = ArrowWriter::try_new(
        File::create(&path).unwrap(),
        batch.schema(),
        Some(properties),
    )
    .unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();
    path
}

fn files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for compression in [Compression::SNAPPY, Compression::UNCOMPRESSED] {
        for version in [WriterVersion::PARQUET_1_0, WriterVersion::PARQUET_2_0] {
            out.push(file(dir, compression, version));
        }
    }
    out
}

#[test]
fn pages_are_decompressed_as_the_crate_decompresses_them() {
    let dir = tempfile::tempdir().unwrap();
    for path in files(dir.path()) {
        let file = ParquetFile::open(&path).unwrap();
        let shared = SharedFile::of(&file);
        let mut kinds = Vec::new();
        for group in file.metadata().row_groups() {
            let rows = group.num_rows() as usize;
            for chunk in group.columns() {
                let mine: Vec<Page> = open(&shared, chunk, rows)
                    .unwrap()
                    .map(Result::unwrap)
                    .collect();
                let theirs: Vec<Page> =
                    SerializedPageReader::new(Arc::new(shared.clone()), chunk, rows, None)
                        .unwrap()
                        .map(Result::unwrap)
                        .collect();
                assert_eq!(mine.len(), theirs.len(), "{path:?}");
                for (a, b) in mine.iter().zip(&theirs) {
                    assert_eq!(a.page_type(), b.page_type());
                    assert_eq!(a.num_values(), b.num_values());
                    assert_eq!(a.buffer(), b.buffer(), "{path:?}");
                    kinds.push(a.page_type());
                }
            }
        }
        assert!(kinds.contains(&parquet::basic::PageType::DICTIONARY_PAGE));
        let data = if path.to_string_lossy().contains("PARQUET_2_0") {
            parquet::basic::PageType::DATA_PAGE_V2
        } else {
            parquet::basic::PageType::DATA_PAGE
        };
        assert!(
            kinds.iter().filter(|&&k| k == data).count() > 10,
            "{path:?}"
        );
    }
}

/// The rows of `selection` of each group by the reader's pages and by
/// the crate's builder under `Selectors`, in batches of 128.
fn both(path: &Path, selection: impl Fn(usize) -> RowSelection) {
    let file = ParquetFile::open(path).unwrap();
    let shared = SharedFile::of(&file);
    let view = ArrowReaderMetadata::try_new(Arc::clone(file.metadata()), ArrowReaderOptions::new())
        .unwrap();
    for group in 0..file.metadata().num_row_groups() {
        let rows = file.metadata().row_group(group).num_rows() as usize;
        let one = Group {
            file: shared.clone(),
            metadata: Arc::clone(file.metadata()),
            group,
        };
        assert_eq!(one.num_rows(), rows);
        let mine: Vec<RecordBatch> = super::rows(&shared, &view, group, selection(rows), 128)
            .unwrap()
            .map(Result::unwrap)
            .collect();
        let theirs: Vec<RecordBatch> =
            ParquetRecordBatchReaderBuilder::new_with_metadata(shared.clone(), view.clone())
                .with_row_groups(vec![group])
                .with_row_selection(selection(rows))
                .with_row_selection_policy(RowSelectionPolicy::Selectors)
                .with_batch_size(128)
                .build()
                .unwrap()
                .map(Result::unwrap)
                .collect();
        assert_eq!(mine, theirs, "{path:?}, group {group}");
    }
}

#[test]
fn rows_are_read_as_the_crate_s_builder_reads_them() {
    let dir = tempfile::tempdir().unwrap();
    for path in files(dir.path()) {
        both(&path, |rows| {
            RowSelection::from(vec![RowSelector::select(rows)])
        });
        // Runs of 300 and 100 rows, which the default policy reads as
        // selectors, and of one and two, which it reads by a mask.
        for (keep, skip) in [(300, 100), (1, 2)] {
            both(&path, |rows| {
                let mut runs = Vec::new();
                let mut at = 0;
                while at < rows {
                    runs.push(RowSelector::skip(skip.min(rows - at)));
                    at += skip.min(rows - at);
                    runs.push(RowSelector::select(keep.min(rows - at)));
                    at += keep.min(rows - at);
                }
                RowSelection::from(runs)
            });
        }
        both(&path, |_| RowSelection::from(vec![]));
    }
}

#[test]
fn a_damaged_stream_is_an_error() {
    let page = Bytes::from_static(&[3, 1, 2, 3, 4, 5]);
    assert!(whole(&page, 0, None).is_err());
    assert!(whole(&page, 7, None).is_err());
    assert_eq!(whole(&page, 6, None).unwrap(), page);
    let good = snap::raw::Encoder::new()
        .compress_vec(b"0123456789")
        .unwrap();
    let mut v2 = vec![9, 9];
    v2.extend_from_slice(&good);
    let v2 = Bytes::from(v2);
    for size in [None, Some(12)] {
        assert_eq!(&whole(&v2, 2, size).unwrap()[..], b"\x09\x090123456789");
    }
    // A size its header gives other than the stream's, and one that
    // leaves the values none, as the crate's codec reads them.
    assert!(whole(&v2, 2, Some(11)).is_err());
    assert!(whole(&v2, 2, Some(13)).is_err());
    assert_eq!(&whole(&v2, 2, Some(2)).unwrap()[..], b"\x09\x09");
}

#[test]
fn a_selection_of_short_runs_skips_the_pages_of_its_long_skip() {
    // Short runs over the first 600 rows of a group of 2 500, which the
    // crate's default policy alone would read by a mask, then a skip of
    // the five whole pages of rows 600 to 2 100, then the rest.
    let dir = tempfile::tempdir().unwrap();
    let path = file(dir.path(), Compression::SNAPPY, WriterVersion::PARQUET_1_0);
    let file = ParquetFile::open(&path).unwrap();
    let shared = SharedFile::of(&file);
    let view = ArrowReaderMetadata::try_new(Arc::clone(file.metadata()), ArrowReaderOptions::new())
        .unwrap();
    let mut runs = Vec::new();
    for _ in 0..300 {
        runs.extend([RowSelector::select(1), RowSelector::skip(1)]);
    }
    runs.extend([RowSelector::skip(1_500), RowSelector::select(400)]);
    let count = |selection: RowSelection| {
        DECOMPRESSED.with(|n| n.set(0));
        let rows: usize = super::rows(&shared, &view, 0, selection, 1_024)
            .unwrap()
            .map(|b| b.unwrap().num_rows())
            .sum();
        (rows, DECOMPRESSED.with(Cell::get))
    };
    let (all_rows, all) = count(RowSelection::from(vec![RowSelector::select(2_500)]));
    let (rows, some) = count(RowSelection::from(runs));
    assert_eq!((all_rows, rows), (2_500, 700));
    // Five pages skipped in each of the two flat columns; the column of
    // lists has no count of rows a page in the format's first version, so
    // its pages are decompressed to count them.
    assert!(some <= all - 2 * 5, "{some} of {all} streams decompressed");
}

/// The file at `path` with the size the header of a data page of its
/// column `id`, in its first group, gives for the page decompressed
/// changed by one.
fn with_a_wrong_size(path: &Path) {
    let file = ParquetFile::open(path).unwrap();
    let chunk = file.metadata().row_group(0).column(1).clone();
    let mut bytes = std::fs::read(path).unwrap();
    let mut at = usize::try_from(chunk.data_page_offset()).unwrap();
    // Field 1, the page's type, then field 2, its size, both i32.
    assert_eq!(bytes[at], 0x15);
    at += 2;
    assert_eq!(bytes[at], 0x15);
    at += 1;
    let first = bytes[at];
    assert!(
        first & 0x7f >= 4,
        "a size whose first byte allows the change"
    );
    bytes[at] = first - 2;
    std::fs::write(path, bytes).unwrap();
}

#[test]
fn a_page_whose_header_gives_another_size_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    for version in [WriterVersion::PARQUET_1_0, WriterVersion::PARQUET_2_0] {
        let path = file(dir.path(), Compression::SNAPPY, version);
        with_a_wrong_size(&path);
        let file = ParquetFile::open(&path).unwrap();
        let shared = SharedFile::of(&file);
        let chunk = file.metadata().row_group(0).column(1);
        let rows = file.metadata().row_group(0).num_rows() as usize;
        let mine: Result<Vec<Page>> = open(&shared, chunk, rows).unwrap().collect();
        let theirs: Result<Vec<Page>> =
            SerializedPageReader::new(Arc::new(shared.clone()), chunk, rows, None)
                .unwrap()
                .collect();
        assert!(theirs.is_err(), "{version:?}");
        let mine = mine.unwrap_err().to_string();
        assert!(
            mine.contains("where its header says"),
            "{version:?}: {mine}"
        );
    }
}

#[test]
fn a_header_s_size_is_read_from_its_first_fields() {
    // Field 1, the type 0, then field 2, the size 4 by zigzag.
    assert_eq!(header_size(&[0x15, 0x00, 0x15, 0x08, 0x15]), Some(4));
    assert_eq!(header_size(&[0x15, 0x06, 0x15, 0x80, 0x01]), Some(64));
    // Field 2 first, by a delta of 2; a negative size; a size of i64; a
    // long field id; cut short; a varint past 32 bits.
    assert_eq!(header_size(&[0x25, 0x08]), Some(4));
    assert_eq!(header_size(&[0x15, 0x00, 0x15, 0x07]), None);
    // A size of five bytes; field 3 first; a long field id before a field
    // 2 that a reader of short ids would take.
    assert_eq!(
        header_size(&[0x15, 0x00, 0x15, 0x80, 0x80, 0x80, 0x80, 0x02]),
        Some(1 << 28)
    );
    assert_eq!(header_size(&[0x35, 0x08]), None);
    assert_eq!(header_size(&[0x05, 0x04, 0x15, 0x08]), None);
    assert_eq!(header_size(&[0x15, 0x00, 0x16, 0x08]), None);
    assert_eq!(header_size(&[0x05, 0x02, 0x00, 0x08]), None);
    assert_eq!(header_size(&[0x15, 0x00, 0x15]), None);
    assert_eq!(
        header_size(&[0x15, 0x00, 0x15, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01]),
        None
    );
}

#[test]
fn a_group_of_few_rows_is_read_by_selectors_too() {
    // A group of 30 rows in pages of 5, and three runs: the trailing skip
    // must outweigh them, with too few rows of the group's own to.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("few.parquet");
    let batch = RecordBatch::try_from_iter([(
        "id",
        Arc::new(Int64Array::from_iter_values(0..30)) as ArrayRef,
    )])
    .unwrap();
    let properties = WriterProperties::builder()
        .set_compression(Compression::SNAPPY)
        .set_dictionary_enabled(false)
        .set_data_page_row_count_limit(5)
        .set_write_batch_size(5)
        .build();
    let mut writer = ArrowWriter::try_new(
        File::create(&path).unwrap(),
        batch.schema(),
        Some(properties),
    )
    .unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();
    let file = ParquetFile::open(&path).unwrap();
    let shared = SharedFile::of(&file);
    let view = ArrowReaderMetadata::try_new(Arc::clone(file.metadata()), ArrowReaderOptions::new())
        .unwrap();
    let selection = RowSelection::from(vec![
        RowSelector::select(1),
        RowSelector::skip(24),
        RowSelector::select(1),
    ]);
    DECOMPRESSED.with(|n| n.set(0));
    let rows: usize = super::rows(&shared, &view, 0, selection, 1_024)
        .unwrap()
        .map(|b| b.unwrap().num_rows())
        .sum();
    // The pages of rows 5 to 20 skipped, not read by a mask.
    assert_eq!(rows, 2);
    assert!(
        DECOMPRESSED.with(Cell::get) <= 3,
        "{}",
        DECOMPRESSED.with(Cell::get)
    );
}
