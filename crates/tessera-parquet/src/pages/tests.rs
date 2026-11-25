use std::fs::File;
use std::path::{Path, PathBuf};

use arrow_array::{ArrayRef, Int64Array, RecordBatch, StringArray};
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_reader::{
    ArrowReaderOptions, ParquetRecordBatchReaderBuilder, RowSelectionPolicy, RowSelector,
};
use parquet::file::properties::{WriterProperties, WriterVersion};

use super::*;
use crate::file::ParquetFile;

const ROWS: usize = 6_000;

/// A file of strings, through a dictionary that grows past its limit and
/// then plain, and of integers, both with `NULL`s, in groups of 2 500
/// rows and pages of 300, of the format's `version`.
fn file(dir: &Path, compression: Compression, version: WriterVersion) -> PathBuf {
    let urls: Vec<Option<String>> = (0..ROWS)
        .map(|i| (i % 7 != 0).then(|| format!("http://example.com/{}/{}", i % 50, i)))
        .collect();
    let ids: Vec<Option<i64>> = (0..ROWS as i64)
        .map(|i| (i % 5 != 0).then_some(i * 3))
        .collect();
    let batch = RecordBatch::try_from_iter([
        ("url", Arc::new(StringArray::from(urls)) as ArrayRef),
        ("id", Arc::new(Int64Array::from(ids)) as ArrayRef),
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
    assert!(whole(&page, 0).is_err());
    assert!(whole(&page, 7).is_err());
    assert_eq!(whole(&page, 6).unwrap(), page);
    let good = snap::raw::Encoder::new()
        .compress_vec(b"0123456789")
        .unwrap();
    let mut v2 = vec![9, 9];
    v2.extend_from_slice(&good);
    assert_eq!(
        &whole(&Bytes::from(v2), 2).unwrap()[..],
        b"\x09\x090123456789"
    );
}
