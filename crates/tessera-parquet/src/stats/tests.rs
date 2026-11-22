use datum_arrow::{Unit, oid};
use parquet::basic::LogicalType;

use super::*;
use crate::column::{ColumnRequest, map_column};
use crate::testing::{self, Column, Options, Rewrite, Values};

fn request(name: &str, type_oid: u32) -> ColumnRequest<'_> {
    ColumnRequest {
        name,
        type_oid,
        typmod: -1,
        unit: None,
        utf8: None,
        server_utf8: true,
    }
}

fn columns() -> Vec<Column> {
    vec![
        Column::new(
            "i",
            Values::Int32(vec![Some(-5), None, Some(7), Some(1), Some(2), Some(3)]),
        ),
        Column::new(
            "u",
            Values::Int32(vec![Some(-1), Some(1), Some(-2), Some(4), Some(5), Some(6)]),
        )
        .logical(LogicalType::Integer {
            bit_width: 32,
            is_signed: false,
        }),
        Column::int64("l", &[10, 20, 30, 40, 50, 60]),
        Column::bytes("s", &[b"", b"", b"", b"b", b"", b"c"]),
    ]
}

#[test]
fn bounds_nulls_and_empties_of_each_group() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.parquet");
    testing::write(&path, &columns(), 3, false).unwrap();
    let file = ParquetFile::open(&path).unwrap();
    let i = map_column(&file, &request("i", oid::INT4)).unwrap();
    let u = map_column(&file, &request("u", oid::INT8)).unwrap();
    let l = map_column(&file, &request("l", oid::INT8)).unwrap();
    let s = map_column(&file, &request("s", oid::TEXT)).unwrap();
    let g0 = group_stats(&file, &i, 0);
    assert_eq!(g0.nulls, Some(1));
    assert_eq!(
        g0.bounds,
        Some(Bounds {
            least: -5,
            greatest: 7
        })
    );
    assert_eq!(
        group_stats(&file, &i, 1).bounds,
        Some(Bounds {
            least: 1,
            greatest: 3
        })
    );
    // Unsigned 32-bit values above 2^31, by the order of their type.
    assert_eq!(
        group_stats(&file, &u, 0).bounds,
        Some(Bounds {
            least: 1,
            greatest: i64::from(u32::MAX)
        })
    );
    assert_eq!(
        group_stats(&file, &l, 1).bounds,
        Some(Bounds {
            least: 40,
            greatest: 60
        })
    );
    let empty = |least_empty, greatest_empty| {
        Some(Empties {
            least_empty,
            greatest_empty,
        })
    };
    assert_eq!(group_stats(&file, &s, 0).empties, empty(true, true));
    assert_eq!(group_stats(&file, &s, 1).empties, empty(true, false));
    assert_eq!(group_stats(&file, &s, 0).bounds, None);
    // A group past the file's says nothing.
    assert_eq!(group_stats(&file, &i, 2), GroupStats::default());
}

#[test]
fn the_old_rule_trusts_signed_values_alone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.parquet");
    testing::write(&path, &columns(), 3, false).unwrap();
    testing::rewrite_footer(
        &path,
        Rewrite {
            old_rule: true,
            drop_null_counts: true,
            ..Rewrite::default()
        },
    )
    .unwrap();
    let file = ParquetFile::open(&path).unwrap();
    let i = map_column(&file, &request("i", oid::INT4)).unwrap();
    let u = map_column(&file, &request("u", oid::INT8)).unwrap();
    let s = map_column(&file, &request("s", oid::TEXT)).unwrap();
    let g0 = group_stats(&file, &i, 0);
    assert_eq!(
        (g0.nulls, g0.bounds),
        (
            None,
            Some(Bounds {
                least: -5,
                greatest: 7
            })
        )
    );
    assert_eq!(group_stats(&file, &u, 0).bounds, None);
    // Old writers wrote wrong least and greatest strings: not trusted.
    assert_eq!(group_stats(&file, &s, 1).empties, None);
}

#[test]
fn newer_fields_without_column_orders_are_not_trusted() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.parquet");
    testing::write(&path, &columns(), 3, false).unwrap();
    // The crate's writer always writes column orders: the footer without
    // them is made in memory, the statistics of the newer fields kept.
    let file = ParquetFile::open(&path).unwrap();
    let i = map_column(&file, &request("i", oid::INT4)).unwrap();
    let meta = file.metadata();
    let fm = meta.file_metadata();
    let without = parquet::file::metadata::ParquetMetaData::new(
        parquet::file::metadata::FileMetaData::new(
            fm.version(),
            fm.num_rows(),
            None,
            None,
            fm.schema_descr_ptr(),
            None,
        ),
        meta.row_groups().to_vec(),
    );
    assert_eq!(stats_of(&without, &i, 0).bounds, None);
    assert_eq!(stats_of(&without, &i, 0).nulls, Some(1));
    // The same from a footer whose column orders were cut.
    testing::rewrite_footer(
        &path,
        Rewrite {
            column_orders: Some(false),
            ..Rewrite::default()
        },
    )
    .unwrap();
    let cut = ParquetFile::open(&path).unwrap();
    let i_cut = map_column(&cut, &request("i", oid::INT4)).unwrap();
    assert!(cut.metadata().file_metadata().column_orders().is_none());
    assert_eq!(group_stats(&cut, &i_cut, 0).bounds, None);
    assert_eq!(group_stats(&cut, &i_cut, 0).nulls, Some(1));
    assert_eq!(
        stats_of(meta, &i, 0).bounds,
        Some(Bounds {
            least: -5,
            greatest: 7
        })
    );
}

#[test]
fn dates_and_times_in_their_stored_unit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.parquet");
    testing::write(
        &path,
        &[
            Column::int32("d", &[15_901, 15_900]).logical(LogicalType::Date),
            Column::int32("u16", &[15_901, 15_902]).logical(LogicalType::Integer {
                bit_width: 16,
                is_signed: false,
            }),
            Column::int64("t", &[1_373_832_000, 1_373_918_399]),
        ],
        2,
        false,
    )
    .unwrap();
    let file = ParquetFile::open(&path).unwrap();
    let d = map_column(&file, &request("d", oid::DATE)).unwrap();
    let u16 = map_column(
        &file,
        &ColumnRequest {
            unit: Some(Unit::Days),
            ..request("u16", oid::DATE)
        },
    )
    .unwrap();
    let t = map_column(
        &file,
        &ColumnRequest {
            unit: Some(Unit::Seconds),
            ..request("t", oid::TIMESTAMP)
        },
    )
    .unwrap();
    assert_eq!(
        group_stats(&file, &d, 0).bounds,
        Some(Bounds {
            least: 15_900,
            greatest: 15_901
        })
    );
    assert_eq!(
        group_stats(&file, &u16, 0).bounds,
        Some(Bounds {
            least: 15_901,
            greatest: 15_902
        })
    );
    assert_eq!(
        group_stats(&file, &t, 0).bounds,
        Some(Bounds {
            least: 1_373_832_000,
            greatest: 1_373_918_399
        })
    );
}

#[test]
fn every_data_page_through_the_dictionary_or_not() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.parquet");
    let values: Vec<i32> = (0..400).map(|v| v % 5).collect();
    let column = || Column::int32("a", &values);
    let s = || Column::bytes("s", &[&b"x"[..]; 400]);
    testing::write(&path, &[column(), s()], 400, true).unwrap();
    let file = ParquetFile::open(&path).unwrap();
    let a = map_column(&file, &request("a", oid::INT4)).unwrap();
    assert!(group_stats(&file, &a, 0).dictionary_only);
    // Plain pages alone.
    testing::write(&path, &[column(), s()], 400, false).unwrap();
    let file = ParquetFile::open(&path).unwrap();
    assert!(
        !group_stats(
            &file,
            &map_column(&file, &request("a", oid::INT4)).unwrap(),
            0
        )
        .dictionary_only
    );
    // A dictionary that grows past its limit falls back to plain pages.
    let many: Vec<i32> = (0..400).collect();
    let options = Options {
        page_rows: Some(50),
        dictionary_bytes: Some(64),
        ..Options::new(400, true)
    };
    testing::write_with(&path, &[Column::int32("a", &many), s()], options).unwrap();
    let file = ParquetFile::open(&path).unwrap();
    assert!(
        !group_stats(
            &file,
            &map_column(&file, &request("a", oid::INT4)).unwrap(),
            0
        )
        .dictionary_only
    );
    // A footer without the statistics of encodings says nothing.
    testing::write(&path, &[column(), s()], 400, true).unwrap();
    testing::rewrite_footer(
        &path,
        Rewrite {
            drop_encoding_stats: true,
            ..Rewrite::default()
        },
    )
    .unwrap();
    let file = ParquetFile::open(&path).unwrap();
    assert!(
        !group_stats(
            &file,
            &map_column(&file, &request("a", oid::INT4)).unwrap(),
            0
        )
        .dictionary_only
    );
    // No statistics written, the pages through the dictionary: the
    // statistics of encodings still say so.
    let options = Options {
        statistics: false,
        ..Options::new(400, true)
    };
    testing::write_with(&path, &[column(), s()], options).unwrap();
    let file = ParquetFile::open(&path).unwrap();
    let only = group_stats(
        &file,
        &map_column(&file, &request("a", oid::INT4)).unwrap(),
        0,
    );
    assert!(only.dictionary_only && only.bounds.is_none());
    // No statistics written: no bounds and no count.
    let options = Options {
        statistics: false,
        ..Options::new(400, false)
    };
    testing::write_with(&path, &[column(), s()], options).unwrap();
    let file = ParquetFile::open(&path).unwrap();
    let none = group_stats(
        &file,
        &map_column(&file, &request("a", oid::INT4)).unwrap(),
        0,
    );
    assert_eq!((none.nulls, none.bounds), (None, None));
}

#[test]
fn the_emptiness_of_strings_by_the_newer_fields_alone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.parquet");
    testing::write(&path, &columns(), 3, false).unwrap();
    testing::rewrite_footer(
        &path,
        Rewrite {
            old_rule: true,
            ..Rewrite::default()
        },
    )
    .unwrap();
    let file = ParquetFile::open(&path).unwrap();
    let s = map_column(&file, &request("s", oid::TEXT)).unwrap();
    assert_eq!(group_stats(&file, &s, 1).empties, None);
    // Without column orders, the newer fields are not trusted either.
    testing::write(&path, &columns(), 3, false).unwrap();
    let file = ParquetFile::open(&path).unwrap();
    let s = map_column(&file, &request("s", oid::TEXT)).unwrap();
    let meta = file.metadata();
    let fm = meta.file_metadata();
    let without = parquet::file::metadata::ParquetMetaData::new(
        parquet::file::metadata::FileMetaData::new(
            fm.version(),
            fm.num_rows(),
            None,
            None,
            fm.schema_descr_ptr(),
            None,
        ),
        meta.row_groups().to_vec(),
    );
    assert_eq!(stats_of(&without, &s, 1).empties, None);
    assert!(stats_of(meta, &s, 1).empties.is_some());
}

#[test]
fn a_least_string_cut_short_is_still_not_empty() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.parquet");
    let options = Options {
        truncate: Some(2),
        ..Options::new(3, false)
    };
    testing::write_with(
        &path,
        &[Column::bytes("s", &[b"abcdefgh", b"bcdefgh"])],
        options,
    )
    .unwrap();
    let file = ParquetFile::open(&path).unwrap();
    let s = map_column(&file, &request("s", oid::TEXT)).unwrap();
    let empties = group_stats(&file, &s, 0).empties.unwrap();
    assert!(!empties.least_empty && !empties.greatest_empty);
}

#[test]
fn bounds_outside_a_mark_s_width_are_not_given() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.parquet");
    let columns = [
        Column::int32("i8", &[1, 300]).logical(LogicalType::Integer {
            bit_width: 8,
            is_signed: true,
        }),
        Column::int32("u16", &[1, 70_000]).logical(LogicalType::Integer {
            bit_width: 16,
            is_signed: false,
        }),
        Column::int32("u8", &[1, 300]).logical(LogicalType::Integer {
            bit_width: 8,
            is_signed: false,
        }),
        Column::int32("i16", &[1, 40_000]).logical(LogicalType::Integer {
            bit_width: 16,
            is_signed: true,
        }),
        Column::int32("ok", &[-128, 127]).logical(LogicalType::Integer {
            bit_width: 8,
            is_signed: true,
        }),
    ];
    testing::write(&path, &columns, 2, false).unwrap();
    let file = ParquetFile::open(&path).unwrap();
    for name in ["i8", "u16", "u8", "i16"] {
        let c = map_column(&file, &request(name, oid::INT4)).unwrap();
        assert_eq!(group_stats(&file, &c, 0).bounds, None, "{name}");
    }
    let ok = map_column(&file, &request("ok", oid::INT2)).unwrap();
    assert_eq!(
        group_stats(&file, &ok, 0).bounds,
        Some(Bounds {
            least: -128,
            greatest: 127
        })
    );
}

#[test]
fn both_names_of_the_dictionary_and_a_plain_page() {
    use parquet::basic::{Encoding, EncodingMask};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.parquet");
    testing::write(&path, &[Column::int32("a", &[1, 1, 2])], 3, true).unwrap();
    let file = ParquetFile::open(&path).unwrap();
    let a = map_column(&file, &request("a", oid::INT4)).unwrap();
    let meta = file.metadata();
    let with = |encodings: &[Encoding]| {
        let chunk = meta
            .row_group(0)
            .column(0)
            .clone()
            .into_builder()
            .set_page_encoding_stats_mask(EncodingMask::new_from_encodings(encodings.iter()))
            .build()
            .unwrap();
        let group = meta
            .row_group(0)
            .clone()
            .into_builder()
            .set_column_metadata(vec![chunk])
            .build()
            .unwrap();
        let rebuilt = parquet::file::metadata::ParquetMetaData::new(
            meta.file_metadata().clone(),
            vec![group],
        );
        stats_of(&rebuilt, &a, 0).dictionary_only
    };
    assert!(with(&[Encoding::PLAIN_DICTIONARY]));
    assert!(with(&[Encoding::RLE_DICTIONARY]));
    assert!(with(&[
        Encoding::PLAIN_DICTIONARY,
        Encoding::RLE_DICTIONARY
    ]));
    assert!(!with(&[Encoding::PLAIN_DICTIONARY, Encoding::PLAIN]));
    assert!(!with(&[Encoding::RLE_DICTIONARY, Encoding::PLAIN]));
    assert!(!with(&[]));
}
