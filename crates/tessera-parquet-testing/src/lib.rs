//! The writer of the Parquet files of `tessera_parquet`'s suite, for the
//! C test module `test/tessera_parquet_test.c`: a file described by a
//! spec in JSON is written through `tessera-parquet`'s writer of test
//! files, and its footer and page headers rewritten as older writers
//! write them. It is a library of its own, so that the wrapper's library
//! carries no writer.
//!
//! A spec is an object:
//!
//! ```json
//! {"group_rows": 2, "dictionary": true, "page_rows": 10,
//!  "dictionary_bytes": 64, "statistics": true, "compression": "snappy",
//!  "truncate": 4, "no_group": false,
//!  "arrow_schema": [{"name": "a", "type": "utf8"}],
//!  "columns": [{"name": "a", "type": "int32", "logical": "int16",
//!               "converted": "INT_16", "optional": true,
//!               "values": [1, null, 3]}]}
//! ```
//!
//! A column's `type` is `bool`, `int32`, `int64`, `int96`, `float`,
//! `double`, `bytes` or `fixed:N`; its values are numbers, booleans,
//! strings for bytes, `{"hex": "ff00"}` for bytes of any value, three
//! words for an `INT96`, or `null`; `"range": N` in place of `values`
//! gives the numbers from 0 to N - 1; `"list": true` makes it a list of
//! one value a row. Its
//! `logical` mark is one of `string`, `date`, `int8`, `int16`, `int32`,
//! `int64`, `uint8`, `uint16`, `uint32`, `uint64`, `timestamp_ms`,
//! `timestamp_us`, `timestamp_ns` with `_utc` for one adjusted to UTC,
//! `time_us`, `json`, `uuid` or `decimal:P:S`; its `converted` mark one of
//! the format's converted types by name.

use std::ffi::c_char;
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;

use parquet::basic::{Compression, ConvertedType, LogicalType, TimeUnit};
use serde_json::Value;
use tessera_parquet::testing::{self, Column, Options, Rewrite, Values};

/// Bytes of the message of a status, its terminator included.
pub const MESSAGE_SIZE: usize = 256;

/// The status of a call that failed, as `TessParquetTestStatus`.
#[repr(C)]
pub struct Status {
    /// A NUL-terminated message for people.
    pub message: [c_char; MESSAGE_SIZE],
}

type Outcome = Result<(), String>;

/// Runs `body`, writing its error or a caught panic into `status`.
///
/// # Safety
///
/// `status` must be null or point to writable memory of a `Status`.
unsafe fn guard(status: *mut Status, body: impl FnOnce() -> Outcome) -> bool {
    let text = match panic::catch_unwind(AssertUnwindSafe(body)) {
        Ok(Ok(())) => return true,
        Ok(Err(text)) => text,
        Err(_) => String::from("a panic in the writer of test files"),
    };
    if !status.is_null() {
        let mut message = [0 as c_char; MESSAGE_SIZE];
        let mut len = text.len().min(MESSAGE_SIZE - 1);
        while !text.is_char_boundary(len) {
            len -= 1;
        }
        for (place, &byte) in message.iter_mut().zip(&text.as_bytes()[..len]) {
            *place = byte as c_char;
        }
        // SAFETY: the caller gives a writable status; its one field is
        // written whole.
        unsafe { std::ptr::addr_of_mut!((*status).message).write(message) };
    }
    false
}

/// # Safety
///
/// `ptr` must point to `len` readable bytes.
unsafe fn text<'a>(ptr: *const c_char, len: usize) -> Result<&'a str, String> {
    if ptr.is_null() {
        return Err(String::from("a null pointer"));
    }
    // SAFETY: the caller gives `len` readable bytes at `ptr`.
    let bytes = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), len) };
    std::str::from_utf8(bytes).map_err(|_| String::from("text that is not UTF-8"))
}

fn logical(name: &str) -> Result<LogicalType, String> {
    let int = |bit_width, is_signed| LogicalType::Integer {
        bit_width,
        is_signed,
    };
    let time = |unit, utc| LogicalType::Timestamp {
        is_adjusted_to_u_t_c: utc,
        unit,
    };
    Ok(match name {
        "string" => LogicalType::String,
        "date" => LogicalType::Date,
        "int8" => int(8, true),
        "int16" => int(16, true),
        "int32" => int(32, true),
        "int64" => int(64, true),
        "uint8" => int(8, false),
        "uint16" => int(16, false),
        "uint32" => int(32, false),
        "uint64" => int(64, false),
        "timestamp_ms" => time(TimeUnit::MILLIS, false),
        "timestamp_us" => time(TimeUnit::MICROS, false),
        "timestamp_ns" => time(TimeUnit::NANOS, false),
        "timestamp_ms_utc" => time(TimeUnit::MILLIS, true),
        "timestamp_us_utc" => time(TimeUnit::MICROS, true),
        "time_us" => LogicalType::Time {
            is_adjusted_to_u_t_c: false,
            unit: TimeUnit::MICROS,
        },
        "json" => LogicalType::Json,
        "uuid" => LogicalType::Uuid,
        other => {
            let parts: Vec<&str> = other.split(':').collect();
            match parts.as_slice() {
                ["decimal", precision, scale] => LogicalType::Decimal {
                    precision: precision
                        .parse()
                        .map_err(|_| format!("a precision {precision}"))?,
                    scale: scale.parse().map_err(|_| format!("a scale {scale}"))?,
                },
                _ => return Err(format!("an unknown logical type {other}")),
            }
        }
    })
}

fn bytes_of(value: &Value) -> Result<Option<Vec<u8>>, String> {
    match value {
        Value::Null => Ok(None),
        Value::String(s) => Ok(Some(s.as_bytes().to_vec())),
        Value::Object(o) => {
            let hex = o
                .get("hex")
                .and_then(Value::as_str)
                .ok_or_else(|| String::from("bytes of an object without hex"))?;
            (0..hex.len())
                .step_by(2)
                .map(|i| {
                    hex.get(i..i + 2)
                        .and_then(|b| u8::from_str_radix(b, 16).ok())
                        .ok_or_else(|| format!("hex that is not bytes: {hex}"))
                })
                .collect::<Result<Vec<u8>, String>>()
                .map(Some)
        }
        other => Err(format!("bytes of {other}")),
    }
}

fn values_of(kind: &str, values: &[Value]) -> Result<Values, String> {
    let ints = |values: &[Value]| -> Result<Vec<Option<i64>>, String> {
        values
            .iter()
            .map(|v| match v {
                Value::Null => Ok(None),
                v => v
                    .as_i64()
                    .map(Some)
                    .ok_or_else(|| format!("an integer of {v}")),
            })
            .collect()
    };
    let floats = |values: &[Value]| -> Result<Vec<Option<f64>>, String> {
        values
            .iter()
            .map(|v| match v {
                Value::Null => Ok(None),
                v => v
                    .as_f64()
                    .map(Some)
                    .ok_or_else(|| format!("a number of {v}")),
            })
            .collect()
    };
    Ok(match kind {
        "bool" => Values::Bool(
            values
                .iter()
                .map(|v| match v {
                    Value::Null => Ok(None),
                    v => v
                        .as_bool()
                        .map(Some)
                        .ok_or_else(|| format!("a boolean of {v}")),
                })
                .collect::<Result<_, String>>()?,
        ),
        "int32" => Values::Int32(
            ints(values)?
                .into_iter()
                .map(|v| v.map(|v| v as i32))
                .collect(),
        ),
        "int64" => Values::Int64(ints(values)?),
        "float" => Values::Float(
            floats(values)?
                .into_iter()
                .map(|v| v.map(|v| v as f32))
                .collect(),
        ),
        "double" => Values::Double(floats(values)?),
        "bytes" => Values::Bytes(values.iter().map(bytes_of).collect::<Result<_, _>>()?),
        "int96" => Values::Int96(
            values
                .iter()
                .map(|v| match v {
                    Value::Null => Ok(None),
                    v => {
                        let words: Vec<u32> = v
                            .as_array()
                            .map(|a| {
                                a.iter()
                                    .filter_map(|w| w.as_u64().map(|w| w as u32))
                                    .collect()
                            })
                            .unwrap_or_default();
                        <[u32; 3]>::try_from(words)
                            .map(Some)
                            .map_err(|_| format!("an INT96 of {v}"))
                    }
                })
                .collect::<Result<_, String>>()?,
        ),
        other => match other.strip_prefix("fixed:").map(str::parse::<i32>) {
            Some(Ok(length)) => Values::Fixed(
                length,
                values.iter().map(bytes_of).collect::<Result<_, _>>()?,
            ),
            _ => return Err(format!("an unknown type {other}")),
        },
    })
}

fn column_of(spec: &Value) -> Result<Column, String> {
    let name = spec
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| String::from("a column without a name"))?;
    let kind = spec
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("column {name} without a type"))?;
    let range: Vec<Value>;
    let values = match spec.get("range").and_then(Value::as_u64) {
        Some(n) => {
            range = (0..n).map(Value::from).collect();
            &range
        }
        None => spec
            .get("values")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("column {name} without values"))?,
    };
    let mut column = Column::new(name, values_of(kind, values)?);
    if let Some(mark) = spec.get("logical").and_then(Value::as_str) {
        column = column.logical(logical(mark)?);
    }
    if let Some(mark) = spec.get("converted").and_then(Value::as_str) {
        let converted: ConvertedType = mark
            .parse()
            .map_err(|_| format!("an unknown converted type {mark}"))?;
        column = column.converted(converted);
    }
    if spec.get("optional").and_then(Value::as_bool) == Some(true) {
        column = column.optional();
    }
    if spec.get("list").and_then(Value::as_bool) == Some(true) {
        column = column.list();
    }
    Ok(column)
}

fn arrow_type(name: &str) -> Result<arrow_schema::DataType, String> {
    use arrow_schema::DataType as D;
    Ok(match name {
        "utf8" => D::Utf8,
        "int64" => D::Int64,
        "int32" => D::Int32,
        "float64" => D::Float64,
        "binary" => D::Binary,
        other => return Err(format!("an unknown Arrow type {other}")),
    })
}

fn options_of(spec: &Value, rows: usize) -> Result<Options, String> {
    let number = |key: &str| spec.get(key).and_then(Value::as_u64).map(|n| n as usize);
    let mut options = Options::new(
        number("group_rows").unwrap_or(rows.max(1)),
        spec.get("dictionary")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    );
    options.page_rows = number("page_rows");
    options.dictionary_bytes = number("dictionary_bytes");
    options.truncate = number("truncate");
    options.no_group = spec.get("no_group").and_then(Value::as_bool) == Some(true);
    if let Some(statistics) = spec.get("statistics").and_then(Value::as_bool) {
        options.statistics = statistics;
    }
    options.compression = match spec.get("compression").and_then(Value::as_str) {
        None | Some("none") => Compression::UNCOMPRESSED,
        Some("snappy") => Compression::SNAPPY,
        Some(other) => return Err(format!("an unknown codec {other}")),
    };
    if let Some(fields) = spec.get("arrow_schema").and_then(Value::as_array) {
        let fields = fields
            .iter()
            .map(|f| {
                let name = f.get("name").and_then(Value::as_str).unwrap_or_default();
                let kind = f.get("type").and_then(Value::as_str).unwrap_or_default();
                Ok(arrow_schema::Field::new(name, arrow_type(kind)?, true))
            })
            .collect::<Result<Vec<_>, String>>()?;
        options.arrow_schema = Some(arrow_schema::Schema::new(fields));
    }
    Ok(options)
}

/// Writes the file the spec describes, as the crate's documentation says.
pub fn write_spec(path: &Path, spec: &str) -> Outcome {
    let spec: Value = serde_json::from_str(spec).map_err(|e| format!("a spec: {e}"))?;
    let columns = spec
        .get("columns")
        .and_then(Value::as_array)
        .ok_or_else(|| String::from("a spec without columns"))?
        .iter()
        .map(column_of)
        .collect::<Result<Vec<_>, String>>()?;
    let first = spec
        .get("columns")
        .and_then(Value::as_array)
        .and_then(|c| c.first());
    let rows = first
        .and_then(|c| c.get("range"))
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .or_else(|| {
            first
                .and_then(|c| c.get("values"))
                .and_then(Value::as_array)
                .map(Vec::len)
        })
        .unwrap_or(0);
    let options = options_of(&spec, rows)?;
    testing::write_with(path, &columns, options).map_err(|e| e.to_string())
}

/// Writes the file at the path of `path_len` bytes that the spec of
/// `spec_len` bytes describes.
///
/// # Safety
///
/// `path` and `spec` must point to as many readable bytes as their
/// lengths say; `status` must be null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_parquet_test_write(
    path: *const c_char,
    path_len: usize,
    spec: *const c_char,
    spec_len: usize,
    status: *mut Status,
) -> bool {
    // SAFETY: as the function's contract says.
    unsafe {
        guard(status, || {
            let path = text(path, path_len)?;
            write_spec(Path::new(path), text(spec, spec_len)?)
        })
    }
}

/// Rewrites the footer of the file at the path of `path_len` bytes: the
/// least and greatest values of the old rule, the statistics of
/// encodings or the counts of `NULL` values dropped, as the flags say, and
/// the codec named for every chunk of leaf `codec_leaf` (-1 for none) set
/// to GZIP.
///
/// # Safety
///
/// `path` must point to `path_len` readable bytes; `status` must be null
/// or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_parquet_test_rewrite(
    path: *const c_char,
    path_len: usize,
    old_rule: bool,
    drop_encoding_stats: bool,
    drop_null_counts: bool,
    codec_leaf: i32,
    status: *mut Status,
) -> bool {
    // SAFETY: as the function's contract says.
    unsafe {
        guard(status, || {
            let path = text(path, path_len)?;
            let rewrite = Rewrite {
                old_rule,
                drop_encoding_stats,
                drop_null_counts,
                codec: usize::try_from(codec_leaf)
                    .ok()
                    .map(|leaf| (leaf, Compression::GZIP(Default::default()))),
                first_group_rows: None,
            };
            testing::rewrite_footer(Path::new(path), rewrite).map_err(|e| e.to_string())
        })
    }
}

/// Heads every dictionary page of the file `PLAIN_DICTIONARY`.
///
/// # Safety
///
/// `path` must point to `path_len` readable bytes; `status` must be null
/// or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tess_parquet_test_head_dictionaries(
    path: *const c_char,
    path_len: usize,
    status: *mut Status,
) -> bool {
    // SAFETY: as the function's contract says.
    unsafe {
        guard(status, || {
            let path = text(path, path_len)?;
            testing::head_dictionaries_plain_dictionary(Path::new(path)).map_err(|e| e.to_string())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_of_no_rows_holds_one_group_or_none() {
        let dir = tempfile::tempdir().unwrap();
        for (no_group, groups) in [(false, 1), (true, 0)] {
            let path = dir.path().join(format!("{no_group}.parquet"));
            let spec = format!(
                r#"{{"no_group": {no_group}, "columns": [{{"name": "a", "type": "int32", "values": []}}]}}"#
            );
            write_spec(&path, &spec).unwrap();
            let file = tessera_parquet::ParquetFile::open(&path).unwrap();
            assert_eq!(
                file.metadata().num_row_groups(),
                groups,
                "no_group {no_group}"
            );
        }
    }

    #[test]
    fn a_spec_written_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.parquet");
        let spec = r#"{"group_rows": 2, "dictionary": true, "compression": "snappy",
            "arrow_schema": [{"name": "a", "type": "utf8"}],
            "columns": [
              {"name": "a", "type": "int32", "logical": "uint16", "values": [1, null, 3]},
              {"name": "s", "type": "bytes", "values": ["x", {"hex": "ff00"}, null]},
              {"name": "t", "type": "int64", "converted": "TIMESTAMP_MILLIS", "values": [1, 2, 3]},
              {"name": "d", "type": "bytes", "logical": "decimal:300:0", "values": ["1", "2", "3"]},
              {"name": "f", "type": "fixed:2", "values": [{"hex": "0102"}, null, "ab"]},
              {"name": "t96", "type": "int96", "values": [[1, 2, 3], null, [4, 5, 6]]},
              {"name": "l", "type": "int32", "list": true, "values": [1, null, 3]},
              {"name": "r", "type": "int64", "range": 3}
            ]}"#;
        write_spec(&path, spec).unwrap();
        let file = tessera_parquet::ParquetFile::open(&path).unwrap();
        assert_eq!((file.groups(), file.rows()), (2, 3));
        let chunk = file.metadata().row_group(0).column(0);
        assert_eq!(chunk.compression(), Compression::SNAPPY);
        let descr = file.metadata().file_metadata().schema_descr();
        assert_eq!(
            descr.column(2).converted_type(),
            ConvertedType::TIMESTAMP_MILLIS
        );
        assert!(
            file.metadata()
                .file_metadata()
                .key_value_metadata()
                .is_some()
        );
    }

    #[test]
    fn specs_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.parquet");
        for spec in [
            "not json",
            r#"{"columns": [{"name": "a", "type": "int128", "values": []}]}"#,
            r#"{"columns": [{"name": "a", "type": "int32", "logical": "nope", "values": [1]}]}"#,
            r#"{"columns": [{"name": "a", "type": "bytes", "values": [{"hex": "zz"}]}]}"#,
            r#"{"compression": "lz4", "columns": [{"name": "a", "type": "int32", "values": [1]}]}"#,
        ] {
            assert!(write_spec(&path, spec).is_err(), "{spec}");
        }
    }

    #[test]
    fn the_entry_points_and_their_status() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.parquet");
        let p = path.to_str().unwrap();
        let spec = r#"{"dictionary": true, "columns": [{"name": "a", "type": "int32", "values": [1, 1, 2]}]}"#;
        let mut status = Status {
            message: [0; MESSAGE_SIZE],
        };
        unsafe {
            assert!(tess_parquet_test_write(
                p.as_ptr().cast(),
                p.len(),
                spec.as_ptr().cast(),
                spec.len(),
                &mut status
            ));
            assert!(tess_parquet_test_head_dictionaries(
                p.as_ptr().cast(),
                p.len(),
                &mut status
            ));
            assert!(tess_parquet_test_rewrite(
                p.as_ptr().cast(),
                p.len(),
                true,
                true,
                true,
                0,
                &mut status
            ));
            assert!(!tess_parquet_test_write(
                p.as_ptr().cast(),
                p.len(),
                "{}".as_ptr().cast(),
                2,
                &mut status
            ));
        }
        let message: String = status
            .message
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8 as char)
            .collect();
        assert!(message.contains("without columns"));
        let file = tessera_parquet::ParquetFile::open(&path).unwrap();
        assert_eq!(
            file.metadata().row_group(0).column(0).compression(),
            Compression::GZIP(Default::default())
        );
    }
}
