use std::path::Path;
use std::sync::Arc;

use datum_arrow::{Failures, TextBytes, Unit, oid};
use parquet::basic::{
    Compression, ConvertedType, LogicalType, Repetition, TimeUnit as ParquetUnit, Type as Physical,
};
use parquet::schema::types::{ColumnDescriptor, ColumnPath, SchemaDescriptor, Type};

use super::*;
use crate::testing::{self, Column, Rewrite, Values};

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

fn file(dir: &Path, columns: &[Column]) -> ParquetFile {
    let path = dir.join("t.parquet");
    testing::write(&path, columns, 4, true).unwrap();
    ParquetFile::open(&path).unwrap()
}

fn code(result: Result<MappedColumn>) -> String {
    result
        .map(|_| String::from("ok"))
        .unwrap_or_else(|e| e.sqlstate().to_owned())
}

fn int32(name: &str, logical: LogicalType) -> Column {
    Column::int32(name, &[1, 2, 3]).logical(logical)
}

#[test]
fn clickbench_s_types() {
    let dir = tempfile::tempdir().unwrap();
    let f = file(
        dir.path(),
        &[
            int32(
                "AdvEngineID",
                LogicalType::Integer {
                    bit_width: 16,
                    is_signed: true,
                },
            ),
            int32(
                "EventDate",
                LogicalType::Integer {
                    bit_width: 16,
                    is_signed: false,
                },
            ),
            Column::int64("EventTime", &[1, 2, 3]),
            Column::int64("UserID", &[1, 2, 3]),
            Column::bytes("URL", &[b"a", b"b", b"c"]),
        ],
    );
    let adv = map_column(&f, &request("AdvEngineID", oid::INT2)).unwrap();
    assert_eq!(
        adv.file_type(),
        FileType::Int {
            bits: 16,
            signed: true
        }
    );
    assert_eq!(adv.plan().failures(), Failures::NONE);
    let date = map_column(
        &f,
        &ColumnRequest {
            unit: Some(Unit::Days),
            ..request("EventDate", oid::DATE)
        },
    )
    .unwrap();
    assert_eq!(date.plan().failures(), Failures::NONE);
    let time = map_column(
        &f,
        &ColumnRequest {
            unit: Some(Unit::Seconds),
            ..request("EventTime", oid::TIMESTAMP)
        },
    )
    .unwrap();
    assert_eq!(time.plan().failures(), Failures::OUT_OF_RANGE);
    assert!(map_column(&f, &request("UserID", oid::INT8)).is_ok());
    let url = map_column(&f, &request("URL", oid::TEXT)).unwrap();
    assert_eq!(url.file_type(), FileType::Bytes);
    assert_eq!(
        url.plan().failures(),
        Failures::NOT_UTF8 | Failures::ZERO_BYTE | Failures::TOO_LARGE
    );
    // A trusted column names the size alone.
    let trusted = map_column(
        &f,
        &ColumnRequest {
            utf8: Some(TextBytes::Trusted),
            ..request("URL", oid::TEXT)
        },
    )
    .unwrap();
    assert_eq!(trusted.plan().failures(), Failures::TOO_LARGE);
}

#[test]
fn every_mapping_of_the_list() {
    let dir = tempfile::tempdir().unwrap();
    let ts = |micros, utc| LogicalType::Timestamp {
        is_adjusted_to_u_t_c: utc,
        unit: if micros {
            ParquetUnit::MICROS
        } else {
            ParquetUnit::MILLIS
        },
    };
    let columns = vec![
        Column::new("b", Values::Bool(vec![Some(true), None, Some(false)])),
        Column::new("f", Values::Float(vec![Some(1.5), Some(2.0), None])),
        Column::new("d", Values::Double(vec![Some(1.5), Some(2.0), Some(3.0)])),
        int32(
            "u8",
            LogicalType::Integer {
                bit_width: 8,
                is_signed: false,
            },
        ),
        int32(
            "i8",
            LogicalType::Integer {
                bit_width: 8,
                is_signed: true,
            },
        ),
        int32(
            "u16",
            LogicalType::Integer {
                bit_width: 16,
                is_signed: false,
            },
        ),
        int32(
            "u32",
            LogicalType::Integer {
                bit_width: 32,
                is_signed: false,
            },
        ),
        Column::int32("i32", &[1, 2, 3]).converted(ConvertedType::INT_32),
        int32("day", LogicalType::Date),
        Column::int32("old_day", &[1, 2, 3]).converted(ConvertedType::DATE),
        Column::int64("ms", &[1, 2, 3]).logical(ts(false, false)),
        Column::int64("us_utc", &[1, 2, 3]).logical(ts(true, true)),
        Column::int64("old_ms", &[1, 2, 3]).converted(ConvertedType::TIMESTAMP_MILLIS),
        Column::int64("old_us", &[1, 2, 3]).converted(ConvertedType::TIMESTAMP_MICROS),
        Column::bytes("s", &[b"a", b"bb", b"ccc"]).logical(LogicalType::String),
        Column::bytes("raw", &[b"a", b"bb", b"ccc"]),
        Column::new(
            "fixed",
            Values::Fixed(2, vec![Some(b"ab".to_vec()), None, None]),
        ),
    ];
    let f = file(dir.path(), &columns);
    let ok = |name, type_oid, typmod| {
        let r = ColumnRequest {
            typmod,
            ..request(name, type_oid)
        };
        code(map_column(&f, &r))
    };
    let varchar10 = 14;
    for (name, type_oid, typmod) in [
        ("b", oid::BOOL, -1),
        ("f", oid::FLOAT4, -1),
        ("f", oid::FLOAT8, -1),
        ("d", oid::FLOAT8, -1),
        ("u8", oid::INT2, -1),
        ("i8", oid::INT2, -1),
        ("u16", oid::INT4, -1),
        ("u16", oid::INT8, -1),
        ("u32", oid::INT8, -1),
        ("i32", oid::INT4, -1),
        ("day", oid::DATE, -1),
        ("old_day", oid::DATE, -1),
        ("ms", oid::TIMESTAMP, -1),
        ("us_utc", oid::TIMESTAMPTZ, 6),
        ("old_ms", oid::TIMESTAMPTZ, -1),
        ("old_us", oid::TIMESTAMPTZ, -1),
        ("s", oid::VARCHAR, varchar10),
        ("s", oid::VARCHAR, -1),
        ("raw", oid::TEXT, -1),
        ("raw", oid::BYTEA, -1),
        ("s", oid::BYTEA, -1),
        ("fixed", oid::BYTEA, -1),
    ] {
        assert_eq!(ok(name, type_oid, typmod), "ok", "{name} as {type_oid}");
    }
    // The older marks read as adjusted to UTC, the newer one not adjusted.
    for (name, type_oid) in [
        ("ms", oid::TIMESTAMPTZ),
        ("old_ms", oid::TIMESTAMP),
        ("u8", oid::BOOL),
        ("u32", oid::INT4),
        ("d", oid::FLOAT4),
    ] {
        assert_eq!(ok(name, type_oid, -1), "42804", "{name} as {type_oid}");
    }
    let nullable = map_column(&f, &request("b", oid::BOOL)).unwrap();
    assert!(nullable.nullable());
    assert!(
        !map_column(&f, &request("d", oid::FLOAT8))
            .unwrap()
            .nullable()
    );
}

#[test]
fn columns_refused_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let f = file(
        dir.path(),
        &[
            Column::int32("i", &[1, 2, 3]),
            Column::int64("l", &[1, 2, 3]),
            int32("day", LogicalType::Date),
            Column::int64("ts", &[1, 2, 3]).logical(LogicalType::Timestamp {
                is_adjusted_to_u_t_c: false,
                unit: ParquetUnit::MICROS,
            }),
            Column::bytes("s", &[b"a", b"b", b"c"]),
            int32(
                "dec",
                LogicalType::Decimal {
                    scale: 2,
                    precision: 9,
                },
            ),
            Column::int64("tod", &[1, 2, 3]).logical(LogicalType::Time {
                is_adjusted_to_u_t_c: false,
                unit: ParquetUnit::MICROS,
            }),
            Column::int64("ns", &[1, 2, 3]).logical(LogicalType::Timestamp {
                is_adjusted_to_u_t_c: false,
                unit: ParquetUnit::NANOS,
            }),
            Column::bytes("json", &[b"1", b"2", b"3"]).logical(LogicalType::Json),
            Column::new("uuid", Values::Fixed(16, vec![Some(vec![0; 16]); 3]))
                .logical(LogicalType::Uuid),
        ],
    );
    let with = |r: ColumnRequest<'_>| code(map_column(&f, &r));
    let days = Some(Unit::Days);
    let seconds = Some(Unit::Seconds);
    let check = Some(TextBytes::Checked);
    let numeric = 1700;
    let cases: Vec<(ColumnRequest<'_>, &str)> = vec![
        (request("missing", oid::INT4), "42703"),
        (request("I", oid::INT4), "42703"),
        (request("dec", oid::INT4), "0A000"),
        (request("tod", oid::INT8), "0A000"),
        (request("ns", oid::TIMESTAMP), "0A000"),
        (request("json", oid::TEXT), "0A000"),
        (request("uuid", oid::BYTEA), "0A000"),
        (request("i", numeric), "0A000"),
        (
            ColumnRequest {
                typmod: 0,
                ..request("ts", oid::TIMESTAMP)
            },
            "0A000",
        ),
        (
            ColumnRequest {
                unit: days,
                ..request("s", oid::TEXT)
            },
            "22023",
        ),
        (
            ColumnRequest {
                utf8: check,
                ..request("i", oid::INT4)
            },
            "22023",
        ),
        (
            ColumnRequest {
                unit: days,
                ..request("day", oid::DATE)
            },
            "22023",
        ),
        (
            ColumnRequest {
                unit: seconds,
                ..request("ts", oid::TIMESTAMP)
            },
            "22023",
        ),
        (
            ColumnRequest {
                unit: days,
                ..request("l", oid::DATE)
            },
            "22023",
        ),
        (
            ColumnRequest {
                unit: seconds,
                ..request("i", oid::TIMESTAMP)
            },
            "22023",
        ),
        (request("l", oid::INT2), "42804"),
        // Two faults each: the first by the order.
        (
            ColumnRequest {
                unit: days,
                ..request("i", numeric)
            },
            "0A000",
        ),
        (
            ColumnRequest {
                utf8: check,
                ..request("l", oid::INT2)
            },
            "22023",
        ),
        // `utf8` applies to text, and bytes alone are checked.
        (
            ColumnRequest {
                utf8: check,
                ..request("i", oid::TEXT)
            },
            "42804",
        ),
        (request("i", oid::TEXT), "42804"),
        // Trusted bytes into a varchar with a length.
        (
            ColumnRequest {
                utf8: Some(TextBytes::Trusted),
                typmod: 7,
                ..request("s", oid::VARCHAR)
            },
            "42804",
        ),
        // Text in a database of another encoding.
        (
            ColumnRequest {
                server_utf8: false,
                ..request("s", oid::TEXT)
            },
            "0A000",
        ),
        (
            ColumnRequest {
                server_utf8: false,
                ..request("s", oid::BYTEA)
            },
            "ok",
        ),
    ];
    for (r, expected) in cases {
        assert_eq!(with(r), expected, "{r:?}");
    }
}

#[test]
fn a_codec_other_than_snappy_is_refused_for_its_column_alone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.parquet");
    testing::write(
        &path,
        &[Column::int32("a", &[1, 2]), Column::int32("b", &[3, 4])],
        2,
        false,
    )
    .unwrap();
    testing::rewrite_footer(
        &path,
        Rewrite {
            codec: Some((1, Compression::GZIP(Default::default()))),
            ..Rewrite::default()
        },
    )
    .unwrap();
    let f = ParquetFile::open(&path).unwrap();
    assert_eq!(code(map_column(&f, &request("b", oid::INT4))), "0A000");
    assert_eq!(code(map_column(&f, &request("a", oid::INT4))), "ok");
}

#[test]
fn groups_lists_and_repeated_columns_are_outside_the_list() {
    let leaf = |name: &str, rep| {
        Arc::new(
            Type::primitive_type_builder(name, Physical::INT32)
                .with_repetition(rep)
                .build()
                .unwrap(),
        )
    };
    let list = Type::group_type_builder("list")
        .with_repetition(Repetition::OPTIONAL)
        .with_logical_type(Some(LogicalType::List))
        .with_fields(vec![leaf("element", Repetition::REPEATED)])
        .build()
        .unwrap();
    let root = Type::group_type_builder("schema")
        .with_fields(vec![
            Arc::new(list),
            leaf("r", Repetition::REPEATED),
            leaf("a", Repetition::REQUIRED),
        ])
        .build()
        .unwrap();
    let schema = SchemaDescriptor::new(Arc::new(root));
    assert_eq!(find(&schema, "list").unwrap_err().sqlstate(), "0A000");
    let (leaf_r, _) = find(&schema, "r").unwrap();
    assert_eq!(classify(&schema.column(leaf_r)), None);
    let (leaf_a, _) = find(&schema, "a").unwrap();
    assert_eq!(
        classify(&schema.column(leaf_a)),
        Some(FileType::Int {
            bits: 32,
            signed: true
        })
    );
    // An unsigned 64-bit integer and an INT96 are outside it too.
    let descr = |t: Type| ColumnDescriptor::new(Arc::new(t), 0, 0, ColumnPath::new(vec![]));
    let u64 = Type::primitive_type_builder("u", Physical::INT64)
        .with_logical_type(Some(LogicalType::Integer {
            bit_width: 64,
            is_signed: false,
        }))
        .build()
        .unwrap();
    assert_eq!(classify(&descr(u64)), None);
    let int96 = Type::primitive_type_builder("t", Physical::INT96)
        .build()
        .unwrap();
    assert_eq!(classify(&descr(int96)), None);
}
