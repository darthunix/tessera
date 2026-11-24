use std::path::Path;
use std::sync::Arc;

use arrow_array::Array;
use datum_arrow::{ReadOutput, TextBytes, oid};
use parquet::basic::LogicalType;
use proptest::prelude::*;

use super::*;
use crate::column::{ColumnRequest, map_column};
use crate::testing::{self, Column, Options};

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

fn rows_of(mask: &[u64], len: usize) -> Vec<usize> {
    (0..len)
        .filter(|&r| mask[r / 64] >> (r % 64) & 1 == 1)
        .collect()
}

/// The `Datum` words of a window's rows, through the reader's plan.
fn datums(reader: &ColumnReader, window: &Window) -> Result<Vec<Option<u64>>> {
    let mut buffers = Vec::new();
    let input = input_of(window.array.as_ref(), &mut buffers)?;
    let len = window.array.len();
    let mut values = vec![0u64; len];
    let mut nulls = vec![0u64; mask_words(len)];
    reader.plan().read(
        &input,
        &every(len),
        &mut ReadOutput::Datums {
            values: &mut values,
            nulls: &mut nulls,
        },
    )?;
    Ok((0..len)
        .map(|r| (nulls[r / 64] >> (r % 64) & 1 == 0).then_some(values[r]))
        .collect())
}

/// The strings of a window's rows, through the reader's plan.
fn strings(reader: &ColumnReader, window: &Window) -> Result<Vec<Option<Vec<u8>>>> {
    let mut buffers = Vec::new();
    let input = input_of(window.array.as_ref(), &mut buffers)?;
    let len = window.array.len();
    let mut values: Vec<&[u8]> = vec![&[]; len];
    let mut nulls = vec![0u64; mask_words(len)];
    reader.plan().read(
        &input,
        &every(len),
        &mut ReadOutput::Bytes {
            values: &mut values,
            nulls: &mut nulls,
        },
    )?;
    Ok((0..len)
        .map(|r| (nulls[r / 64] >> (r % 64) & 1 == 0).then(|| values[r].to_vec()))
        .collect())
}

fn write_ints(dir: &Path, values: &[i32], page_rows: usize) -> ParquetFile {
    write_ints_by(
        dir,
        values,
        page_rows,
        parquet::basic::Compression::UNCOMPRESSED,
    )
}

fn write_ints_by(
    dir: &Path,
    values: &[i32],
    page_rows: usize,
    compression: parquet::basic::Compression,
) -> ParquetFile {
    let path = dir.join("ints.parquet");
    let column = Column::new(
        "a",
        testing::Values::Int32(
            values
                .iter()
                .enumerate()
                .map(|(i, &v)| (i % 7 != 3).then_some(v))
                .collect(),
        ),
    );
    let options = Options {
        page_rows: Some(page_rows),
        compression,
        ..Options::new(values.len(), false)
    };
    testing::write_with(&path, &[column], options).unwrap();
    ParquetFile::open(&path).unwrap()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// Windows read forward over a selection, some of them passed over
    /// as no consumer asks for them, and windows read again out of order
    /// for rows asked: each gives the values of its rows, in their order,
    /// `NULL` where the file holds one; a window after a gap gives every
    /// selected row, as a forward one does.
    #[test]
    fn windows_forward_and_out_of_order(
        rows in 1usize..700,
        selected in prop::collection::vec(any::<u64>(), 11),
        window in 1usize..200,
        batch in 1usize..90,
        page_rows in 1usize..120,
        again in prop::collection::vec(any::<u64>(), 4),
        snappy in any::<bool>(),
        passed in any::<u64>(),
    ) {
        let dir = tempfile::tempdir().unwrap();
        let values: Vec<i32> = (0..rows as i32).map(|v| v * 3 - 500).collect();
        let codec = if snappy {
            parquet::basic::Compression::SNAPPY
        } else {
            parquet::basic::Compression::UNCOMPRESSED
        };
        let file = write_ints_by(dir.path(), &values, page_rows, codec);
        let column = map_column(&file, &request("a", oid::INT4)).unwrap();
        let selection: Arc<[u64]> = selected[..mask_words(rows)].to_vec().into();
        let mut reader = ColumnReader::new(&file, &column, 0, Arc::clone(&selection), batch).unwrap();
        let expect = |row: usize| (row % 7 != 3).then_some(i64::from(values[row]) as u64);
        let mut start = 0;
        let mut windows = Vec::new();
        while start < rows {
            let len = window.min(rows - start);
            // The window's index decides whether it is passed over.
            let index = (start / window) % 64;
            if passed >> index & 1 == 1 {
                start += len;
                continue;
            }
            // Asked for its first selected row alone: a forward window
            // gives every selected row whatever it is asked.
            let sel = bits(&selection, start, len);
            let first: Vec<u64> = sel.iter().scan(false, |found, &word| {
                let bit = if *found { 0 } else { word.isolate_lowest_one() };
                *found |= bit != 0;
                Some(bit)
            }).collect();
            let got = reader.read(start, len, &first).unwrap();
            let held = rows_of(&got.rows, len);
            prop_assert_eq!(&got.rows, &sel);
            let made = datums(&reader, &got).unwrap();
            prop_assert_eq!(made.len(), held.len());
            for (value, row) in made.iter().zip(&held) {
                prop_assert_eq!(*value, expect(start + row));
            }
            windows.push((start, len));
            start += len;
        }
        // Each window read again, behind the forward reader now, for some
        // of its selected rows.
        for (start, len) in windows {
            let sel = bits(&selection, start, len);
            let asked: Vec<u64> = sel.iter().zip(again.iter().cycle()).map(|(s, a)| s & a).collect();
            let got = reader.read(start, len, &asked).unwrap();
            prop_assert_eq!(&got.rows, &asked);
            let made = datums(&reader, &got).unwrap();
            let held = rows_of(&asked, len);
            prop_assert_eq!(made.len(), held.len());
            for (value, row) in made.iter().zip(&held) {
                prop_assert_eq!(*value, expect(start + row));
            }
        }
    }
}

#[test]
fn bytes_marked_as_a_string_are_read_unchecked() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.parquet");
    let strings_written: Vec<&[u8]> = vec![b"ok", b"\xff\xfe", b"fine", b"a\0b", b"end"];
    for dictionary in [false, true] {
        let column = Column::bytes("s", &strings_written).logical(LogicalType::String);
        testing::write(&path, &[column], 5, dictionary).unwrap();
        let file = ParquetFile::open(&path).unwrap();
        let checked = map_column(&file, &request("s", oid::TEXT)).unwrap();
        // The rows that are not UTF-8 or hold a zero byte left out.
        let selection: Arc<[u64]> = vec![0b10101].into();
        let mut reader = ColumnReader::new(&file, &checked, 0, Arc::clone(&selection), 8).unwrap();
        let window = reader.read(0, 5, &[0b10101]).unwrap();
        assert_eq!(
            window.array.data_type().is_dictionary_key_type()
                || matches!(window.array.data_type(), DataType::Dictionary(..)),
            dictionary
        );
        let made = strings(&reader, &window).unwrap();
        assert_eq!(
            made,
            vec![
                Some(b"ok".to_vec()),
                Some(b"fine".to_vec()),
                Some(b"end".to_vec())
            ]
        );
        // Every row read: the crate checked nothing, the plan refuses.
        let selection: Arc<[u64]> = vec![0b11111].into();
        let mut reader = ColumnReader::new(&file, &checked, 0, selection, 8).unwrap();
        let window = reader.read(0, 5, &[0b11111]).unwrap();
        let error = strings(&reader, &window).unwrap_err();
        assert_eq!((error.sqlstate(), error.row()), ("22021", Some(1)));
        // Trusted, every row is its bytes.
        let trusted = map_column(
            &file,
            &ColumnRequest {
                utf8: Some(TextBytes::Trusted),
                ..request("s", oid::TEXT)
            },
        )
        .unwrap();
        let selection: Arc<[u64]> = vec![0b11111].into();
        let mut reader = ColumnReader::new(&file, &trusted, 0, selection, 8).unwrap();
        let window = reader.read(0, 5, &[0b11111]).unwrap();
        let made = strings(&reader, &window).unwrap();
        assert_eq!(made.iter().flatten().count(), 5);
        assert_eq!(made[1].as_deref(), Some(&b"\xff\xfe"[..]));
    }
}

#[test]
fn broken_calls_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let file = write_ints(dir.path(), &[1, 2, 3, 4], 2);
    let column = map_column(&file, &request("a", oid::INT4)).unwrap();
    let all: Arc<[u64]> = vec![0b1111].into();
    let code = |r: Result<ColumnReader>| r.err().map(|e| e.sqlstate().to_owned());
    assert_eq!(
        code(ColumnReader::new(&file, &column, 1, Arc::clone(&all), 4)).as_deref(),
        Some("XX000")
    );
    assert_eq!(
        code(ColumnReader::new(
            &file,
            &column,
            0,
            Arc::from(Vec::new()),
            4
        ))
        .as_deref(),
        Some("XX000")
    );
    assert_eq!(
        code(ColumnReader::new(&file, &column, 0, Arc::clone(&all), 0)).as_deref(),
        Some("XX000")
    );
    let some: Arc<[u64]> = vec![0b0101].into();
    let mut reader = ColumnReader::new(&file, &column, 0, some, 4).unwrap();
    assert_eq!(reader.read(2, 3, &[0b111]).unwrap_err().sqlstate(), "XX000");
    assert_eq!(reader.read(0, 4, &[]).unwrap_err().sqlstate(), "XX000");
    assert_eq!(
        reader.read(0, 4, &[0b0010]).unwrap_err().sqlstate(),
        "XX000"
    );
    assert_eq!(reader.read(0, 4, &[0b0101]).unwrap().array.len(), 2);
}

#[test]
fn a_file_changed_after_a_read_and_a_damaged_page() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let file = write_ints(dir.path(), &(0..100).collect::<Vec<_>>(), 10);
    let column = map_column(&file, &request("a", oid::INT4)).unwrap();
    let all: Arc<[u64]> = every(100).into();
    let mut reader = ColumnReader::new(&file, &column, 0, Arc::clone(&all), 16).unwrap();
    reader.read(0, 50, &every(50)).unwrap();
    let path = dir.path().join("ints.parquet");
    let mut out = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    out.write_all(b"x").unwrap();
    drop(out);
    assert_eq!(
        reader.read(50, 50, &every(50)).unwrap_err().sqlstate(),
        "55000"
    );

    // A window of pages read before the change is not checked again: one
    // page of every row, read with the first window, gives the second,
    // which the crate decodes from the page it holds, arrays of 16 rows at
    // a time, whatever the file is now; a window behind, which reads the
    // page again, fails.
    let page = dir.path().join("one-page");
    std::fs::create_dir(&page).unwrap();
    let file = write_ints(&page, &(0..100).collect::<Vec<_>>(), 100);
    let column = map_column(&file, &request("a", oid::INT4)).unwrap();
    let mut reader = ColumnReader::new(&file, &column, 0, Arc::clone(&all), 16).unwrap();
    reader.read(0, 50, &every(50)).unwrap();
    let reads = reader.shared.reads();
    let mut out = std::fs::OpenOptions::new()
        .append(true)
        .open(page.join("ints.parquet"))
        .unwrap();
    out.write_all(b"x").unwrap();
    drop(out);
    let later = reader.read(50, 40, &every(40)).unwrap();
    assert_eq!(reader.shared.reads(), reads);
    let expect = |row: usize| (row % 7 != 3).then_some(row as u64);
    assert_eq!(
        datums(&reader, &later).unwrap(),
        (50..90).map(expect).collect::<Vec<_>>()
    );
    assert_eq!(
        reader.read(0, 50, &every(50)).unwrap_err().sqlstate(),
        "55000"
    );

    // A page header overwritten: the bytes cannot be decoded.
    let path = dir.path().join("bad.parquet");
    testing::write(
        &path,
        &[Column::int32("a", &(0..100).collect::<Vec<_>>())],
        100,
        false,
    )
    .unwrap();
    let opened = ParquetFile::open(&path).unwrap();
    let start = opened.metadata().row_group(0).column(0).data_page_offset() as usize;
    let mut bytes = std::fs::read(&path).unwrap();
    for byte in &mut bytes[start..start + 8] {
        *byte = 0xff;
    }
    // The same size, the time kept: the footer as it was.
    std::fs::write(&path, &bytes).unwrap();
    let file = ParquetFile::open(&path).unwrap();
    let column = map_column(&file, &request("a", oid::INT4)).unwrap();
    let mut reader = ColumnReader::new(&file, &column, 0, every(100).into(), 16).unwrap();
    assert_eq!(
        reader.read(0, 100, &every(100)).unwrap_err().sqlstate(),
        "XX001"
    );
}

#[test]
fn every_type_through_its_plan() {
    use parquet::basic::{ConvertedType, TimeUnit as PUnit};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("all.parquet");
    let ts = |unit| LogicalType::Timestamp {
        is_adjusted_to_u_t_c: false,
        unit,
    };
    let columns = vec![
        Column::new(
            "b",
            testing::Values::Bool(vec![Some(true), Some(false), None]),
        ),
        Column::int32("i8", &[-3, 4, 5]).converted(ConvertedType::INT_8),
        Column::int32("i16", &[-300, 4, 5]).converted(ConvertedType::INT_16),
        Column::int32("u8", &[200, 4, 5]).converted(ConvertedType::UINT_8),
        Column::int32("u16", &[60_000, 4, 5]).converted(ConvertedType::UINT_16),
        Column::int32("u32", &[-1, 4, 5]).converted(ConvertedType::UINT_32),
        Column::new(
            "f",
            testing::Values::Float(vec![Some(1.5), Some(-2.0), None]),
        ),
        Column::new(
            "d",
            testing::Values::Double(vec![Some(1.5), Some(-2.0), None]),
        ),
        Column::int32("day", &[10_957, 10_958, 0]).logical(LogicalType::Date),
        Column::int64("ms", &[946_684_800_000, 946_684_800_001, 0]).logical(ts(PUnit::MILLIS)),
        Column::int64("us", &[946_684_800_000_000, 1, 0]).logical(ts(PUnit::MICROS)),
        Column::new(
            "fx",
            testing::Values::Fixed(2, vec![Some(b"ab".to_vec()), Some(b"cd".to_vec()), None]),
        ),
    ];
    testing::write(&path, &columns, 3, false).unwrap();
    let file = ParquetFile::open(&path).unwrap();
    let datum = |name: &str, type_oid| {
        let column = map_column(&file, &request(name, type_oid)).unwrap();
        let mut reader = ColumnReader::new(&file, &column, 0, every(3).into(), 8).unwrap();
        let window = reader.read(0, 3, &every(3)).unwrap();
        datums(&reader, &window).unwrap()
    };
    let neg = |v: i64| v as u64;
    assert_eq!(datum("b", oid::BOOL), vec![Some(1), Some(0), None]);
    assert_eq!(
        datum("i8", oid::INT2),
        vec![Some(neg(-3)), Some(4), Some(5)]
    );
    assert_eq!(
        datum("i16", oid::INT2),
        vec![Some(neg(-300)), Some(4), Some(5)]
    );
    assert_eq!(datum("u8", oid::INT2), vec![Some(200), Some(4), Some(5)]);
    assert_eq!(
        datum("u16", oid::INT4),
        vec![Some(60_000), Some(4), Some(5)]
    );
    assert_eq!(
        datum("u32", oid::INT8),
        vec![Some(u64::from(u32::MAX)), Some(4), Some(5)]
    );
    assert_eq!(
        datum("f", oid::FLOAT4)[1],
        Some(neg(i64::from((-2.0f32).to_bits() as i32)))
    );
    assert_eq!(datum("f", oid::FLOAT8)[0], Some(1.5f64.to_bits()));
    assert_eq!(datum("d", oid::FLOAT8)[1], Some((-2.0f64).to_bits()));
    assert_eq!(datum("day", oid::DATE)[..2], [Some(0), Some(1)]);
    assert_eq!(datum("ms", oid::TIMESTAMP)[..2], [Some(0), Some(1000)]);
    assert_eq!(datum("us", oid::TIMESTAMP)[0], Some(0));
    let column = map_column(&file, &request("fx", oid::BYTEA)).unwrap();
    let mut reader = ColumnReader::new(&file, &column, 0, every(3).into(), 8).unwrap();
    let window = reader.read(0, 3, &every(3)).unwrap();
    assert_eq!(
        strings(&reader, &window).unwrap(),
        vec![Some(b"ab".to_vec()), Some(b"cd".to_vec()), None]
    );
}

#[test]
fn arrays_sliced_and_of_another_key() {
    use arrow_array::{DictionaryArray, FixedSizeBinaryArray, Int8Array, LargeBinaryArray};
    let fixed = FixedSizeBinaryArray::try_from_iter([b"ab", b"cd", b"ef"].into_iter()).unwrap();
    let sliced = fixed.slice(1, 2);
    let mut buffers = Vec::new();
    let input = input_of(&sliced, &mut buffers).unwrap();
    let plan = datum_arrow::ReadPlan::new(
        &DataType::FixedSizeBinary(2),
        datum_arrow::PgType::Bytea,
        datum_arrow::ReadOptions {
            unit: None,
            text: None,
            server_utf8: true,
        },
    )
    .unwrap();
    let mut values: Vec<&[u8]> = vec![&[]; 2];
    let mut nulls = [0u64; 1];
    plan.read(
        &input,
        &[0b11],
        &mut ReadOutput::Bytes {
            values: &mut values,
            nulls: &mut nulls,
        },
    )
    .unwrap();
    assert_eq!(values, vec![&b"cd"[..], &b"ef"[..]]);
    let keys = Int8Array::from(vec![0i8, 0]);
    let other =
        DictionaryArray::try_new(keys, Arc::new(LargeBinaryArray::from(vec![&b"x"[..]]))).unwrap();
    assert_eq!(
        input_of(&other, &mut Vec::new()).unwrap_err().sqlstate(),
        "XX000"
    );
    assert_eq!(
        input_of(&arrow_array::NullArray::new(1), &mut Vec::new())
            .unwrap_err()
            .sqlstate(),
        "XX000"
    );
}

#[test]
fn windows_of_no_row_and_a_first_window_in_the_middle() {
    let dir = tempfile::tempdir().unwrap();
    let values: Vec<i32> = (0..300).collect();
    let file = write_ints(dir.path(), &values, 40);
    let column = map_column(&file, &request("a", oid::INT4)).unwrap();
    let expect = |row: usize| (row % 7 != 3).then_some(row as u64);
    // Rows 100 and on selected; the first window asked starts at 150.
    let mut selection = vec![0u64; mask_words(300)];
    for row in 100..300 {
        selection[row / 64] |= 1 << (row % 64);
    }
    let selection: Arc<[u64]> = selection.into();
    let mut reader = ColumnReader::new(&file, &column, 0, Arc::clone(&selection), 16).unwrap();
    let window = reader.read(150, 50, &every(50)).unwrap();
    let made = datums(&reader, &window).unwrap();
    assert_eq!(made, (150..200).map(expect).collect::<Vec<_>>());
    // The next window forward, then one of no selected row out of order.
    let window = reader.read(200, 100, &every(100)).unwrap();
    assert_eq!(
        datums(&reader, &window).unwrap(),
        (200..300).map(expect).collect::<Vec<_>>()
    );
    let none = reader.read(0, 100, &[0, 0]).unwrap();
    assert_eq!(none.array.len(), 0);
    // A reader whose first window holds no selected row.
    let mut reader = ColumnReader::new(&file, &column, 0, selection, 16).unwrap();
    assert_eq!(reader.read(0, 64, &[0]).unwrap().array.len(), 0);
    let window = reader.read(64, 64, &[u64::MAX << 36]).unwrap();
    assert_eq!(
        datums(&reader, &window).unwrap(),
        (100..128).map(expect).collect::<Vec<_>>()
    );
}

/// The strings of a test column: bytes of every value, some repeated so a
/// dictionary has work, `NULL` every eleventh row.
fn some_strings(rows: usize, seed: &[u8]) -> Vec<Option<Vec<u8>>> {
    (0..rows)
        .map(|r| {
            (r % 11 != 5).then(|| {
                let len = (seed[r % seed.len()] % 9) as usize;
                (0..len)
                    .map(|i| seed[(r * 7 + i) % seed.len()] % 5 + b'a' - 1 + (r % 3) as u8 * 60)
                    .collect()
            })
        })
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    /// Strings read window by window forward and again out of order, from
    /// plain pages, through a dictionary, and through a dictionary that
    /// falls back to plain pages, compressed by Snappy or not: each gives
    /// the bytes of its rows, in their order.
    #[test]
    fn strings_forward_and_out_of_order(
        rows in 1usize..500,
        seed in prop::collection::vec(any::<u8>(), 1..40),
        selected in prop::collection::vec(any::<u64>(), 8),
        window in 1usize..150,
        batch in 1usize..70,
        layout in 0usize..3,
        snappy in any::<bool>(),
        again in prop::collection::vec(any::<u64>(), 3),
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.parquet");
        let strings = some_strings(rows, &seed);
        let options = Options {
            page_rows: Some(37),
            dictionary_bytes: (layout == 2).then_some(48),
            compression: if snappy { parquet::basic::Compression::SNAPPY } else { parquet::basic::Compression::UNCOMPRESSED },
            ..Options::new(rows, layout > 0)
        };
        testing::write_with(&path, &[Column::new("s", testing::Values::Bytes(strings.clone()))], options).unwrap();
        let file = ParquetFile::open(&path).unwrap();
        let column = map_column(
            &file,
            &ColumnRequest { utf8: Some(TextBytes::Trusted), ..request("s", oid::TEXT) },
        )
        .unwrap();
        let selection: Arc<[u64]> = selected[..mask_words(rows)].to_vec().into();
        let mut reader = ColumnReader::new(&file, &column, 0, Arc::clone(&selection), batch).unwrap();
        let mut start = 0;
        let mut windows = Vec::new();
        while start < rows {
            let len = window.min(rows - start);
            let got = reader.read(start, len, &bits(&selection, start, len)).unwrap();
            let held = rows_of(&got.rows, len);
            let made = strings_of(&reader, &got).unwrap();
            prop_assert_eq!(made.len(), held.len());
            for (value, row) in made.iter().zip(&held) {
                prop_assert_eq!(value, &strings[start + row]);
            }
            windows.push((start, len));
            start += len;
        }
        for (start, len) in windows.into_iter().rev() {
            let sel = bits(&selection, start, len);
            let asked: Vec<u64> = sel.iter().zip(again.iter().cycle()).map(|(s, a)| s & a).collect();
            let got = reader.read(start, len, &asked).unwrap();
            let made = strings_of(&reader, &got).unwrap();
            for (value, row) in made.iter().zip(rows_of(&asked, len)) {
                prop_assert_eq!(value, &strings[start + row]);
            }
        }
    }
}

/// The strings of a window's rows, as `strings` gives them, through the
/// window's own array whatever plan the reader has.
fn strings_of(reader: &ColumnReader, window: &Window) -> Result<Vec<Option<Vec<u8>>>> {
    strings(reader, window)
}

#[test]
fn a_read_that_fails_under_a_changed_file_is_the_change() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ints.parquet");
    testing::write(
        &path,
        &[Column::int32("a", &(0..100).collect::<Vec<_>>())],
        100,
        false,
    )
    .unwrap();
    let file = ParquetFile::open(&path).unwrap();
    let column = map_column(&file, &request("a", oid::INT4)).unwrap();
    let mut reader = ColumnReader::new(&file, &column, 0, every(100).into(), 16).unwrap();
    // Its pages overwritten in place, the size kept, the time moved on.
    let start = file.metadata().row_group(0).column(0).data_page_offset() as usize;
    let mut bytes = std::fs::read(&path).unwrap();
    for byte in &mut bytes[start..start + 8] {
        *byte = 0xff;
    }
    std::fs::write(&path, &bytes).unwrap();
    let handle = std::fs::File::options().write(true).open(&path).unwrap();
    handle
        .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(60))
        .unwrap();
    assert_eq!(
        reader.read(0, 100, &every(100)).unwrap_err().sqlstate(),
        "55000"
    );
    // The forward reader was dropped: the next ask builds another, and
    // fails the same way.
    assert_eq!(
        reader.read(0, 100, &every(100)).unwrap_err().sqlstate(),
        "55000"
    );
}

#[test]
fn a_panic_of_the_decoders_is_damaged_bytes_or_the_change() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ints.parquet");
    testing::write(
        &path,
        &[Column::int32("a", &(0..100).collect::<Vec<_>>())],
        100,
        false,
    )
    .unwrap();
    testing::head_data_page_rle_dictionary(&path, 0, 0).unwrap();
    let file = ParquetFile::open(&path).unwrap();
    let column = map_column(&file, &request("a", oid::INT4)).unwrap();
    let mut reader = ColumnReader::new(&file, &column, 0, every(100).into(), 16).unwrap();
    assert_eq!(
        reader.read(0, 100, &every(100)).unwrap_err().sqlstate(),
        "XX001"
    );
    let handle = std::fs::File::options().write(true).open(&path).unwrap();
    handle
        .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(60))
        .unwrap();
    assert_eq!(
        reader.read(0, 100, &every(100)).unwrap_err().sqlstate(),
        "55000"
    );
}

#[test]
fn a_failed_forward_read_leaves_no_reader_behind() {
    use parquet::file::metadata::{PageIndexPolicy, ParquetMetaDataReader};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ints.parquet");
    let values: Vec<i32> = (0..100).collect();
    let options = Options {
        page_rows: Some(50),
        ..Options::new(100, false)
    };
    testing::write_with(&path, &[Column::int32("a", &values)], options).unwrap();
    // The second page's header damaged, the time kept.
    let pages = ParquetMetaDataReader::new()
        .with_page_index_policy(PageIndexPolicy::Required)
        .parse_and_finish(&std::fs::File::open(&path).unwrap())
        .unwrap();
    let second = pages.offset_index().unwrap()[0][0].page_locations()[1].offset as usize;
    let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
    let mut bytes = std::fs::read(&path).unwrap();
    for byte in &mut bytes[second..second + 6] {
        *byte = 0xff;
    }
    std::fs::write(&path, &bytes).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(modified)
        .unwrap();
    let file = ParquetFile::open(&path).unwrap();
    let column = map_column(&file, &request("a", oid::INT4)).unwrap();
    let mut reader = ColumnReader::new(&file, &column, 0, every(100).into(), 10).unwrap();
    assert_eq!(
        reader.read(0, 100, &every(100)).unwrap_err().sqlstate(),
        "XX001"
    );
    // A forward ask from the first row reads the first page's rows again.
    let window = reader.read(0, 50, &every(50)).unwrap();
    let expect: Vec<Option<u64>> = (0..50u64).map(Some).collect();
    assert_eq!(datums(&reader, &window).unwrap(), expect);
}

#[test]
fn a_read_the_system_fails_is_an_error_of_its_reads() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ints.parquet");
    testing::write(
        &path,
        &[Column::int32("a", &(0..10).collect::<Vec<_>>())],
        10,
        false,
    )
    .unwrap();
    let file = ParquetFile::open(&path).unwrap();
    let column = map_column(&file, &request("a", oid::INT4)).unwrap();
    // The footer of the file over a directory: every read fails.
    let directory = std::fs::File::open(dir.path()).unwrap();
    let broken = file.with_handle(directory).unwrap();
    let mut reader = ColumnReader::new(&broken, &column, 0, every(10).into(), 16).unwrap();
    assert_eq!(
        reader.read(0, 10, &every(10)).unwrap_err().sqlstate(),
        "58030"
    );
}

/// The bits of `mask` from `start` for `len` rows, a row at a time: the
/// reference of [`bits`].
fn bits_one_by_one(mask: &[u64], start: usize, len: usize) -> Vec<u64> {
    let mut out = vec![0u64; mask_words(len)];
    for row in 0..len {
        let at = start + row;
        if mask[at / 64] >> (at % 64) & 1 == 1 {
            out[row / 64] |= 1 << (row % 64);
        }
    }
    out
}

/// The runs of a selection found a row at a time: the reference of
/// [`selectors`].
fn selectors_one_by_one(rows: &[u64], from: usize, total: usize) -> Vec<RowSelector> {
    let selected = |row: usize| row >= from && rows[row / 64] >> (row % 64) & 1 == 1;
    let mut out = Vec::new();
    let mut row = 0;
    while row < total {
        let this = selected(row);
        let run = (row..total).find(|&r| selected(r) != this).unwrap_or(total);
        out.push(if this {
            RowSelector::select(run - row)
        } else {
            RowSelector::skip(run - row)
        });
        row = run;
    }
    out
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// The masks a word at a time give what a row at a time gives, for
    /// masks of long runs and of single rows, from any row.
    #[test]
    fn masks_by_words_are_masks_by_rows(
        words in prop::collection::vec(
            prop_oneof![Just(0u64), Just(u64::MAX), any::<u64>()],
            1..12,
        ),
        start in 0usize..700,
        len in 0usize..700,
        from in 0usize..800,
    ) {
        let rows = words.len() * 64;
        let start = start.min(rows);
        let len = len.min(rows - start);
        prop_assert_eq!(bits(&words, start, len), bits_one_by_one(&words, start, len));
        let total = rows - rows.min(len % 64);
        let by_words: Vec<RowSelector> = selectors(&words, from, total).into();
        let by_rows: Vec<RowSelector> =
            RowSelection::from(selectors_one_by_one(&words, from, total)).into();
        prop_assert_eq!(by_words, by_rows);
    }
}

/// The windows the forward reader gives of a chunk of strings encoded
/// through its dictionary share the dictionary's buffers, so that a test
/// of strings checks each value of it once for the chunk.
#[test]
fn windows_of_a_chunk_share_its_dictionary() {
    use arrow_array::Array as _;
    use arrow_array::cast::AsArray as _;
    use arrow_array::types::Int32Type;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.parquet");
    let rows = 3000;
    let strings: Vec<Option<Vec<u8>>> = (0..rows)
        .map(|i| Some(format!("v{}", i % 50).into_bytes()))
        .collect();
    let options = Options {
        page_rows: Some(500),
        ..Options::new(rows, true)
    };
    testing::write_with(
        &path,
        &[Column::new("s", testing::Values::Bytes(strings))],
        options,
    )
    .unwrap();
    let file = ParquetFile::open(&path).unwrap();
    let column = map_column(&file, &request("s", oid::TEXT)).unwrap();
    let selection: Arc<[u64]> = vec![u64::MAX; mask_words(rows)].into();
    let mut reader = ColumnReader::new(&file, &column, 0, Arc::clone(&selection), 1024).unwrap();
    let mut dictionaries = Vec::new();
    for start in (0..rows).step_by(1024) {
        let len = 1024.min(rows - start);
        let got = reader
            .read(start, len, &bits(&selection, start, len))
            .unwrap();
        let values = Arc::clone(got.array.as_dictionary::<Int32Type>().values());
        assert!(values.len() <= 50);
        dictionaries.push(values);
    }
    assert!(dictionaries.len() == 3);
    // Each window an array of its own over the same buffers.
    let buffers = |d: &Arc<dyn arrow_array::Array>| {
        let b = d.as_binary::<i64>();
        (b.value_offsets().as_ptr(), b.values().as_ptr(), b.len())
    };
    for pair in dictionaries.windows(2) {
        assert_eq!(buffers(&pair[0]), buffers(&pair[1]));
    }
}

/// Windows read forward through a chunk of many pages hold the pages of
/// their own reads alone, not every page read before them: windows as
/// long as the reader's arrays, and shorter ones that leave rows of an
/// array for the next window, each with the rows of the chunk.
#[test]
fn windows_forward_hold_the_pages_of_their_own_reads() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.parquet");
    let rows = 6000;
    let strings: Vec<Option<Vec<u8>>> = (0..rows)
        .map(|i| Some(format!("a value of row {i:0>20}").into_bytes()))
        .collect();
    let options = Options {
        page_rows: Some(50),
        ..Options::new(rows, false)
    };
    testing::write_with(
        &path,
        &[Column::new("s", testing::Values::Bytes(strings.clone()))],
        options,
    )
    .unwrap();
    let file = ParquetFile::open(&path).unwrap();
    let column = map_column(&file, &request("s", oid::BYTEA)).unwrap();
    let selection: Arc<[u64]> = vec![u64::MAX; mask_words(rows)].into();
    for window in [200, 70] {
        let mut reader = ColumnReader::new(&file, &column, 0, Arc::clone(&selection), 200).unwrap();
        let mut start = 0;
        while start < rows {
            let len = window.min(rows - start);
            let got = reader
                .read(start, len, &bits(&selection, start, len))
                .unwrap();
            let views = got
                .array
                .as_any()
                .downcast_ref::<arrow_array::BinaryViewArray>()
                .unwrap();
            // An array of 200 rows spans five pages of 50, a window at most
            // two arrays; a page more where a page is cut between them.
            assert!(
                views.data_buffers().len() <= 12,
                "window at {start}: {}",
                views.data_buffers().len()
            );
            assert_eq!(
                strings_of(&reader, &got).unwrap(),
                strings[start..start + len]
            );
            start += len;
        }
    }
}

/// A chunk of strings whose writer fell back from its dictionary to plain
/// pages is read as an array of views, so that no array of its plain
/// pages gets a dictionary of its own and no value is copied: NULLs and
/// values of 0, 12, 13 and a thousand bytes, through the dictionary and
/// in the plain pages; a footer that does not say how the pages are
/// encoded leaves it read through the dictionary.
#[test]
fn a_chunk_that_falls_back_to_plain_pages_is_read_plain() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.parquet");
    let rows = 2000;
    let strings: Vec<Option<Vec<u8>>> = (0..rows)
        .map(|i| match i % 5 {
            0 => None,
            1 => Some(Vec::new()),
            2 => Some(format!("{i:0>12}").into_bytes()),
            3 => Some(format!("{i:0>13}").into_bytes()),
            _ => Some(format!("{i:0>1000}").into_bytes()),
        })
        .collect();
    let options = Options {
        page_rows: Some(100),
        dictionary_bytes: Some(4096),
        ..Options::new(rows, true)
    };
    testing::write_with(
        &path,
        &[Column::new("s", testing::Values::Bytes(strings.clone()))],
        options,
    )
    .unwrap();
    for drop_encoding_stats in [false, true] {
        if drop_encoding_stats {
            testing::rewrite_footer(
                &path,
                testing::Rewrite {
                    drop_encoding_stats,
                    ..testing::Rewrite::default()
                },
            )
            .unwrap();
        }
        let file = ParquetFile::open(&path).unwrap();
        let chunk = file.metadata().row_group(0).column(0);
        assert!(chunk.dictionary_page_offset().is_some());
        assert_eq!(
            crate::stats::through_dictionary(chunk),
            (!drop_encoding_stats).then_some(false)
        );
        let column = map_column(&file, &request("s", oid::TEXT)).unwrap();
        let selection: Arc<[u64]> = vec![u64::MAX; mask_words(rows)].into();
        // Windows that straddle the arrays the crate gives.
        let mut reader = ColumnReader::new(&file, &column, 0, Arc::clone(&selection), 700).unwrap();
        for start in (0..rows).step_by(500) {
            let got = reader
                .read(start, 500, &bits(&selection, start, 500))
                .unwrap();
            let expected = match drop_encoding_stats {
                true => {
                    DataType::Dictionary(Box::new(DataType::Int32), Box::new(DataType::LargeBinary))
                }
                false => DataType::BinaryView,
            };
            assert_eq!(got.array.data_type(), &expected);
            let made = strings_of(&reader, &got).unwrap();
            assert_eq!(made, strings[start..start + 500]);
        }
    }
}
