use std::collections::BTreeSet;

use datum_arrow::{ReadOutput, Unit, mask_words, oid};
use parquet::basic::LogicalType;

use super::*;
use crate::column::{ColumnRequest, map_column};
use crate::reader::{ColumnReader, input_of};
use crate::testing::{self, Column, Values};

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

fn every(len: usize) -> Vec<u64> {
    let mut mask = vec![0u64; mask_words(len)];
    for row in 0..len {
        mask[row / 64] |= 1 << (row % 64);
    }
    mask
}

/// The distinct `Datum` words of every row of the chunk, by the crate's
/// reader.
fn rows_read(file: &ParquetFile, column: &MappedColumn, group: usize) -> BTreeSet<u64> {
    let rows = file.group_rows(group).unwrap() as usize;
    let mut reader = ColumnReader::new(file, column, group, every(rows).into(), 1024).unwrap();
    let window = reader.read(0, rows, &every(rows)).unwrap();
    let input = input_of(window.array.as_ref()).unwrap();
    let mut values = vec![0u64; rows];
    let mut nulls = vec![0u64; mask_words(rows)];
    reader
        .plan()
        .read(
            &input,
            &every(rows),
            &mut ReadOutput::Datums {
                values: &mut values,
                nulls: &mut nulls,
            },
        )
        .unwrap();
    values.into_iter().collect()
}

#[test]
fn integers_against_the_crate_s_reader() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.parquet");
    let ints: Vec<i32> = (0..300).map(|v| (v * 7) % 11 - 5).collect();
    let unsigned: Vec<i32> = (0..300)
        .map(|v| if v % 2 == 0 { -1 } else { v % 3 })
        .collect();
    let days: Vec<i32> = (0..300).map(|v| 15_900 + v % 4).collect();
    let longs: Vec<i64> = (0..300).map(|v| 1_373_832_000 + v % 9).collect();
    let columns = [
        Column::int32("i", &ints),
        Column::int32("u", &unsigned).logical(LogicalType::Integer {
            bit_width: 32,
            is_signed: false,
        }),
        Column::int32("d", &days).logical(LogicalType::Integer {
            bit_width: 16,
            is_signed: false,
        }),
        Column::int64("t", &longs),
    ];
    for headed in [false, true] {
        testing::write(&path, &columns, 150, true).unwrap();
        if headed {
            testing::head_dictionaries_plain_dictionary(&path).unwrap();
        }
        let file = ParquetFile::open(&path).unwrap();
        for (name, type_oid, unit) in [
            ("i", oid::INT4, None),
            ("u", oid::INT8, None),
            ("d", oid::DATE, Some(Unit::Days)),
            ("t", oid::TIMESTAMP, Some(Unit::Seconds)),
        ] {
            let column = map_column(
                &file,
                &ColumnRequest {
                    unit,
                    ..request(name, type_oid)
                },
            )
            .unwrap();
            for group in 0..2 {
                let Some(Dictionary::Integers(values)) =
                    read_dictionary(&file, &column, group).unwrap()
                else {
                    panic!("{name}: no dictionary of integers");
                };
                // Each stored value, as the plan makes it, against the rows.
                let plan = column.plan();
                let made: BTreeSet<u64> = values
                    .iter()
                    .map(|&v| {
                        let one = [v];
                        let mut out = [0u64; 1];
                        let mut nulls = [0u64; 1];
                        let input = stored_input(column.file_type(), &one);
                        plan.read(
                            &input.as_input(),
                            &[1],
                            &mut ReadOutput::Datums {
                                values: &mut out,
                                nulls: &mut nulls,
                            },
                        )
                        .unwrap();
                        out[0]
                    })
                    .collect();
                assert_eq!(
                    made,
                    rows_read(&file, &column, group),
                    "{name} {group} {headed}"
                );
                assert_eq!(made.len(), values.len(), "{name}: a value twice");
            }
        }
    }
}

/// A one-value column of the stored type of `file_type`, owned.
enum Stored {
    I32(Vec<i32>),
    U32(Vec<u32>),
    U16(Vec<u16>),
    I64(Vec<i64>),
}

impl Stored {
    fn as_input(&self) -> datum_arrow::Input<'_> {
        let values = match self {
            Stored::I32(v) => datum_arrow::Values::Int32(v),
            Stored::U32(v) => datum_arrow::Values::UInt32(v),
            Stored::U16(v) => datum_arrow::Values::UInt16(v),
            Stored::I64(v) => datum_arrow::Values::Int64(v),
        };
        datum_arrow::Input::Plain(datum_arrow::Column {
            len: 1,
            values,
            nulls: None,
        })
    }
}

fn stored_input(file_type: FileType, one: &[i64; 1]) -> Stored {
    match file_type {
        FileType::Int {
            bits: 32,
            signed: false,
        } => Stored::U32(vec![one[0] as u32]),
        FileType::Int {
            bits: 16,
            signed: false,
        } => Stored::U16(vec![one[0] as u16]),
        FileType::Int { bits: 64, .. } | FileType::Timestamp { .. } => Stored::I64(vec![one[0]]),
        _ => Stored::I32(vec![one[0] as i32]),
    }
}

#[test]
fn strings_against_the_crate_s_reader() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.parquet");
    let words: Vec<&[u8]> = vec![b"", b"google", b"\xff\xfe", b"yandex", b"", b"google"];
    let column = Column::bytes("s", &words).logical(LogicalType::String);
    for (headed, snappy) in [(false, false), (true, false), (false, true), (true, true)] {
        let codec = if snappy {
            parquet::basic::Compression::SNAPPY
        } else {
            parquet::basic::Compression::UNCOMPRESSED
        };
        let options = testing::Options {
            compression: codec,
            ..testing::Options::new(6, true)
        };
        testing::write_with(&path, std::slice::from_ref(&column), options).unwrap();
        if headed {
            testing::head_dictionaries_plain_dictionary(&path).unwrap();
        }
        let file = ParquetFile::open(&path).unwrap();
        let mapped = map_column(
            &file,
            &ColumnRequest {
                utf8: Some(datum_arrow::TextBytes::Trusted),
                ..request("s", oid::TEXT)
            },
        )
        .unwrap();
        let Some(Dictionary::Bytes(values)) = read_dictionary(&file, &mapped, 0).unwrap() else {
            panic!("no dictionary of strings");
        };
        let distinct: BTreeSet<Vec<u8>> = words.iter().map(|w| w.to_vec()).collect();
        assert_eq!(values.iter().cloned().collect::<BTreeSet<_>>(), distinct);
        assert_eq!(values.len(), distinct.len());
        // The crate's reader reads the chunk's rows as they were written,
        // the repeats and the bytes that are not UTF-8 among them.
        let mut reader = ColumnReader::new(&file, &mapped, 0, every(6).into(), 8).unwrap();
        let window = reader.read(0, 6, &every(6)).unwrap();
        let input = input_of(window.array.as_ref()).unwrap();
        let mut rows: Vec<&[u8]> = vec![&[]; 6];
        let mut nulls = [0u64; 1];
        reader
            .plan()
            .read(
                &input,
                &every(6),
                &mut ReadOutput::Bytes {
                    values: &mut rows,
                    nulls: &mut nulls,
                },
            )
            .unwrap();
        assert_eq!(rows, words);
        // Every row is a value of the dictionary.
        assert!(rows.iter().all(|r| values.iter().any(|v| v == r)));
    }
}

#[test]
fn no_dictionary_and_types_the_source_does_not_check() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.parquet");
    let columns = [
        Column::int32("i", &[1, 2, 3]),
        Column::new("f", Values::Double(vec![Some(1.0), Some(2.0), Some(1.0)])),
    ];
    testing::write(&path, &columns, 3, false).unwrap();
    let file = ParquetFile::open(&path).unwrap();
    let i = map_column(&file, &request("i", oid::INT4)).unwrap();
    assert_eq!(read_dictionary(&file, &i, 0).unwrap(), None);
    assert_eq!(read_dictionary(&file, &i, 5).unwrap(), None);
    testing::write(&path, &columns, 3, true).unwrap();
    let file = ParquetFile::open(&path).unwrap();
    let f = map_column(&file, &request("f", oid::FLOAT8)).unwrap();
    assert_eq!(read_dictionary(&file, &f, 0).unwrap(), None);
    let i = map_column(&file, &request("i", oid::INT4)).unwrap();
    assert_eq!(
        read_dictionary(&file, &i, 0).unwrap(),
        Some(Dictionary::Integers(vec![1, 2, 3]))
    );
}

#[test]
fn values_shorter_than_their_page_says() {
    assert_eq!(
        plain_bytes(&[3, 0, 0, 0, b'a'], 1).unwrap_err().sqlstate(),
        "XX001"
    );
    assert_eq!(plain_bytes(&[1, 0, 0], 1).unwrap_err().sqlstate(), "XX001");
    assert_eq!(
        plain_bytes(&[1, 0, 0, 0, b'a', 0, 0, 0, 0], 2).unwrap(),
        vec![b"a".to_vec(), vec![]]
    );
    let int = FileType::Int {
        bits: 32,
        signed: true,
    };
    assert_eq!(
        plain_integers(&[1, 0, 0], 1, 4, int)
            .unwrap_err()
            .sqlstate(),
        "XX001"
    );
    assert_eq!(plain_integers(&[255; 4], 1, 4, int).unwrap(), vec![-1]);
    let unsigned = FileType::Int {
        bits: 32,
        signed: false,
    };
    assert_eq!(
        plain_integers(&[255; 4], 1, 4, unsigned).unwrap(),
        vec![i64::from(u32::MAX)]
    );
    let long = FileType::Int {
        bits: 64,
        signed: true,
    };
    assert_eq!(plain_integers(&[255; 8], 1, 8, long).unwrap(), vec![-1]);
}

#[test]
fn values_outside_a_mark_s_width_are_narrowed_as_the_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.parquet");
    let columns = [
        Column::int32("i8", &[300, 1, 300]).logical(LogicalType::Integer {
            bit_width: 8,
            is_signed: true,
        }),
        Column::int32("u16", &[70_000, 2, 70_000]).logical(LogicalType::Integer {
            bit_width: 16,
            is_signed: false,
        }),
    ];
    let options = testing::Options {
        compression: parquet::basic::Compression::SNAPPY,
        ..testing::Options::new(3, true)
    };
    testing::write_with(&path, &columns, options).unwrap();
    testing::head_dictionaries_plain_dictionary(&path).unwrap();
    let file = ParquetFile::open(&path).unwrap();
    for (name, type_oid) in [("i8", oid::INT2), ("u16", oid::INT4)] {
        let column = map_column(&file, &request(name, type_oid)).unwrap();
        let Some(Dictionary::Integers(values)) = read_dictionary(&file, &column, 0).unwrap() else {
            panic!("{name}: no dictionary");
        };
        let stored: BTreeSet<u64> = values.iter().map(|&v| v as u64).collect();
        assert_eq!(stored, rows_read(&file, &column, 0), "{name}");
    }
    assert_eq!(
        narrowed(
            300,
            FileType::Int {
                bits: 8,
                signed: true
            }
        ),
        44
    );
    assert_eq!(
        narrowed(
            -1,
            FileType::Int {
                bits: 8,
                signed: false
            }
        ),
        255
    );
    assert_eq!(
        narrowed(
            70_000,
            FileType::Int {
                bits: 16,
                signed: true
            }
        ),
        4464
    );
    assert_eq!(
        narrowed(
            -1,
            FileType::Int {
                bits: 16,
                signed: false
            }
        ),
        65_535
    );
    assert_eq!(
        narrowed(
            -1,
            FileType::Int {
                bits: 32,
                signed: false
            }
        ),
        i64::from(u32::MAX)
    );
    assert_eq!(narrowed(-1, FileType::Date), -1);
}

#[test]
fn a_dictionary_read_under_a_change_and_one_damaged_before() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.parquet");
    let write = || {
        testing::write(&path, &[Column::int32("a", &[1, 2, 1])], 3, true).unwrap();
    };
    let damage = |file: &ParquetFile| {
        let start = file
            .metadata()
            .row_group(0)
            .column(0)
            .dictionary_page_offset()
            .unwrap() as usize;
        let mut bytes = std::fs::read(&path).unwrap();
        for byte in &mut bytes[start..start + 4] {
            *byte = 0xff;
        }
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::fs::write(&path, &bytes).unwrap();
        modified
    };
    let move_time = || {
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(60))
            .unwrap();
    };
    // Damaged under an open file, the time moved: the change.
    write();
    let file = ParquetFile::open(&path).unwrap();
    let column = map_column(&file, &request("a", oid::INT4)).unwrap();
    damage(&file);
    move_time();
    assert_eq!(
        read_dictionary(&file, &column, 0).unwrap_err().sqlstate(),
        "55000"
    );
    // The time moved alone: the change, after a read that succeeds.
    write();
    let file = ParquetFile::open(&path).unwrap();
    let column = map_column(&file, &request("a", oid::INT4)).unwrap();
    move_time();
    assert_eq!(
        read_dictionary(&file, &column, 0).unwrap_err().sqlstate(),
        "55000"
    );
    // Damaged before the file was opened: damaged bytes.
    write();
    let first = ParquetFile::open(&path).unwrap();
    let kept = damage(&first);
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(kept)
        .unwrap();
    let file = ParquetFile::open(&path).unwrap();
    let column = map_column(&file, &request("a", oid::INT4)).unwrap();
    assert_eq!(
        read_dictionary(&file, &column, 0).unwrap_err().sqlstate(),
        "XX001"
    );
}
