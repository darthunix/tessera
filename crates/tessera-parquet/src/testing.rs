//! The writer of the suites' Parquet files, behind the feature
//! `test-files`: columns of any physical type and mark, `NULL` values,
//! groups of rows, dictionary and plain pages, statistics written or not,
//! footers rewritten to the old rule of least and greatest values, and
//! dictionary pages headed `PLAIN_DICTIONARY` as older writers head them.
//!
//! It uses the `parquet` crate's writer of columns, which takes bytes as
//! they are, so that a column marked as a string can hold bytes that are
//! not UTF-8.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use parquet::basic::{ConvertedType, Encoding, LogicalType, Repetition, Type as Physical};
use parquet::data_type::{
    BoolType, ByteArray, ByteArrayType, DoubleType, FixedLenByteArray, FixedLenByteArrayType,
    FloatType, Int32Type, Int64Type,
};
use parquet::file::metadata::{
    FileMetaData, ParquetMetaData, ParquetMetaDataOptions, ParquetMetaDataReader,
    ParquetMetaDataWriter, RowGroupMetaData,
};
use parquet::file::properties::{EnabledStatistics, WriterProperties};
use parquet::file::statistics::{Statistics, ValueStatistics};
use parquet::file::writer::SerializedFileWriter;
use parquet::schema::types::Type;

use crate::error::{self, Error, INTERNAL_ERROR, Result};

/// The values of a column, one a row, `None` for `NULL`.
#[derive(Clone, Debug)]
pub enum Values {
    /// `BOOLEAN`.
    Bool(Vec<Option<bool>>),
    /// `INT32`.
    Int32(Vec<Option<i32>>),
    /// `INT64`.
    Int64(Vec<Option<i64>>),
    /// `FLOAT`.
    Float(Vec<Option<f32>>),
    /// `DOUBLE`.
    Double(Vec<Option<f64>>),
    /// `BYTE_ARRAY`.
    Bytes(Vec<Option<Vec<u8>>>),
    /// `FIXED_LEN_BYTE_ARRAY` of the given length.
    Fixed(i32, Vec<Option<Vec<u8>>>),
}

impl Values {
    fn len(&self) -> usize {
        match self {
            Values::Bool(v) => v.len(),
            Values::Int32(v) => v.len(),
            Values::Int64(v) => v.len(),
            Values::Float(v) => v.len(),
            Values::Double(v) => v.len(),
            Values::Bytes(v) | Values::Fixed(_, v) => v.len(),
        }
    }

    fn physical(&self) -> Physical {
        match self {
            Values::Bool(_) => Physical::BOOLEAN,
            Values::Int32(_) => Physical::INT32,
            Values::Int64(_) => Physical::INT64,
            Values::Float(_) => Physical::FLOAT,
            Values::Double(_) => Physical::DOUBLE,
            Values::Bytes(_) => Physical::BYTE_ARRAY,
            Values::Fixed(..) => Physical::FIXED_LEN_BYTE_ARRAY,
        }
    }
}

/// A column of a test file: its name, its mark and its values.
#[derive(Clone, Debug)]
pub struct Column {
    /// The column's name.
    pub name: String,
    /// The logical type, the newer mark.
    pub logical: Option<LogicalType>,
    /// The converted type, the older mark.
    pub converted: ConvertedType,
    /// Whether the column is optional, so that it may hold `NULL`.
    pub optional: bool,
    /// The values.
    pub values: Values,
}

impl Column {
    /// A column of `values`, unmarked, required unless a value is `None`.
    pub fn new(name: &str, values: Values) -> Self {
        let optional = match &values {
            Values::Bool(v) => v.iter().any(Option::is_none),
            Values::Int32(v) => v.iter().any(Option::is_none),
            Values::Int64(v) => v.iter().any(Option::is_none),
            Values::Float(v) => v.iter().any(Option::is_none),
            Values::Double(v) => v.iter().any(Option::is_none),
            Values::Bytes(v) | Values::Fixed(_, v) => v.iter().any(Option::is_none),
        };
        Column {
            name: name.to_owned(),
            logical: None,
            converted: ConvertedType::NONE,
            optional,
            values,
        }
    }

    /// An unmarked required `INT32` column.
    pub fn int32(name: &str, values: &[i32]) -> Self {
        Column::new(
            name,
            Values::Int32(values.iter().copied().map(Some).collect()),
        )
    }

    /// An unmarked required `INT64` column.
    pub fn int64(name: &str, values: &[i64]) -> Self {
        Column::new(
            name,
            Values::Int64(values.iter().copied().map(Some).collect()),
        )
    }

    /// An unmarked `BYTE_ARRAY` column of strings.
    pub fn bytes(name: &str, values: &[&[u8]]) -> Self {
        Column::new(
            name,
            Values::Bytes(values.iter().map(|v| Some(v.to_vec())).collect()),
        )
    }

    /// The same column with the newer mark `logical`, and the older mark
    /// the format derives from it.
    pub fn logical(mut self, logical: LogicalType) -> Self {
        self.converted = ConvertedType::from(Some(logical.clone()));
        self.logical = Some(logical);
        self
    }

    /// The same column with the older mark `converted` alone.
    pub fn converted(mut self, converted: ConvertedType) -> Self {
        self.logical = None;
        self.converted = converted;
        self
    }

    /// The same column, optional.
    pub fn optional(mut self) -> Self {
        self.optional = true;
        self
    }

    fn schema(&self) -> Result<Type> {
        let mut builder = Type::primitive_type_builder(&self.name, self.values.physical())
            .with_repetition(if self.optional {
                Repetition::OPTIONAL
            } else {
                Repetition::REQUIRED
            })
            .with_logical_type(self.logical.clone())
            .with_converted_type(self.converted);
        if let Values::Fixed(length, _) = &self.values {
            builder = builder.with_length(*length);
        }
        if let Some(LogicalType::Decimal { precision, scale }) = &self.logical {
            builder = builder.with_precision(*precision).with_scale(*scale);
        }
        builder.build().map_err(|e| error::damaged(&e))
    }
}

/// How a test file is written.
#[derive(Clone, Debug)]
pub struct Options {
    /// The rows of a group, the last group taking what is left.
    pub group_rows: usize,
    /// Whether the pages are encoded through a dictionary where they can.
    pub dictionary: bool,
    /// The rows of a data page at most, for files of several pages.
    pub page_rows: Option<usize>,
    /// The bytes of a dictionary page at most, past which a writer falls
    /// back to plain pages.
    pub dictionary_bytes: Option<usize>,
    /// Whether the least and greatest values and the counts of `NULL`
    /// values are written.
    pub statistics: bool,
    /// The codec of the pages.
    pub compression: parquet::basic::Compression,
    /// An Arrow schema a writer keeps in the file's metadata, as
    /// `ARROW:schema`, which the reader ignores.
    pub arrow_schema: Option<arrow_schema::Schema>,
    /// The bytes past which a least or greatest string is cut, so that
    /// the greatest is a bound past the rows.
    pub truncate: Option<usize>,
}

impl Options {
    /// Groups of `group_rows`, dictionaries as `dictionary` says, and
    /// statistics.
    pub fn new(group_rows: usize, dictionary: bool) -> Self {
        Options {
            group_rows,
            dictionary,
            page_rows: None,
            dictionary_bytes: None,
            statistics: true,
            compression: parquet::basic::Compression::UNCOMPRESSED,
            arrow_schema: None,
            truncate: None,
        }
    }
}

/// Writes `columns`, all of the same rows, to a new file at `path`, in
/// groups of `group_rows` rows, encoded through dictionaries where
/// `dictionary` says.
///
/// # Errors
///
/// `XX000` for columns of different counts of rows, `58030` for a file
/// that cannot be written.
pub fn write(path: &Path, columns: &[Column], group_rows: usize, dictionary: bool) -> Result<()> {
    write_with(path, columns, Options::new(group_rows, dictionary))
}

/// Writes `columns` as [`write`] does, under `options`.
///
/// # Errors
///
/// Those of [`write`].
pub fn write_with(path: &Path, columns: &[Column], options: Options) -> Result<()> {
    let rows = columns.first().map_or(0, |c| c.values.len());
    if columns.iter().any(|c| c.values.len() != rows) || options.group_rows == 0 {
        return Err(Error::new(INTERNAL_ERROR, "columns of different rows"));
    }
    let fields = columns
        .iter()
        .map(|c| c.schema().map(Arc::new))
        .collect::<Result<Vec<_>>>()?;
    let schema = Type::group_type_builder("schema")
        .with_fields(fields)
        .build()
        .map_err(|e| error::damaged(&e))?;
    let mut properties = WriterProperties::builder()
        .set_dictionary_enabled(options.dictionary)
        .set_statistics_enabled(if options.statistics {
            EnabledStatistics::Chunk
        } else {
            EnabledStatistics::None
        });
    if let Some(page_rows) = options.page_rows {
        properties = properties
            .set_data_page_row_count_limit(page_rows)
            .set_write_batch_size(page_rows);
    }
    if let Some(bytes) = options.dictionary_bytes {
        properties = properties.set_dictionary_page_size_limit(bytes);
    }
    properties = properties
        .set_compression(options.compression)
        .set_statistics_truncate_length(options.truncate);
    if let Some(schema) = &options.arrow_schema {
        let encoded = parquet::arrow::encode_arrow_schema(schema);
        properties =
            properties.set_key_value_metadata(Some(vec![parquet::file::metadata::KeyValue::new(
                String::from("ARROW:schema"),
                encoded,
            )]));
    }
    let file = fs::File::create(path).map_err(|e| error::io(&e, "create test file"))?;
    let mut writer =
        SerializedFileWriter::new(file, Arc::new(schema), Arc::new(properties.build()))
            .map_err(|e| error::damaged(&e))?;
    let mut start = 0;
    while start < rows || (rows == 0 && start == 0) {
        let end = (start + options.group_rows).min(rows);
        let mut group = writer.next_row_group().map_err(|e| error::damaged(&e))?;
        for column in columns {
            let Some(mut out) = group.next_column().map_err(|e| error::damaged(&e))? else {
                return Err(Error::new(INTERNAL_ERROR, "fewer columns than the schema"));
            };
            write_values(&mut out, &column.values, start, end).map_err(|e| error::damaged(&e))?;
            out.close().map_err(|e| error::damaged(&e))?;
        }
        group.close().map_err(|e| error::damaged(&e))?;
        if rows == 0 {
            break;
        }
        start = end;
    }
    writer.close().map_err(|e| error::damaged(&e))?;
    Ok(())
}

fn write_values(
    out: &mut parquet::file::writer::SerializedColumnWriter<'_>,
    values: &Values,
    start: usize,
    end: usize,
) -> parquet::errors::Result<()> {
    fn split<T: Clone>(values: &[Option<T>]) -> (Vec<T>, Vec<i16>) {
        let defs = values.iter().map(|v| i16::from(v.is_some())).collect();
        (values.iter().flatten().cloned().collect(), defs)
    }
    match values {
        Values::Bool(v) => {
            let (vals, defs) = split(&v[start..end]);
            out.typed::<BoolType>()
                .write_batch(&vals, Some(&defs), None)?;
        }
        Values::Int32(v) => {
            let (vals, defs) = split(&v[start..end]);
            out.typed::<Int32Type>()
                .write_batch(&vals, Some(&defs), None)?;
        }
        Values::Int64(v) => {
            let (vals, defs) = split(&v[start..end]);
            out.typed::<Int64Type>()
                .write_batch(&vals, Some(&defs), None)?;
        }
        Values::Float(v) => {
            let (vals, defs) = split(&v[start..end]);
            out.typed::<FloatType>()
                .write_batch(&vals, Some(&defs), None)?;
        }
        Values::Double(v) => {
            let (vals, defs) = split(&v[start..end]);
            out.typed::<DoubleType>()
                .write_batch(&vals, Some(&defs), None)?;
        }
        Values::Bytes(v) => {
            let (vals, defs) = split(&v[start..end]);
            let vals: Vec<ByteArray> = vals.into_iter().map(ByteArray::from).collect();
            out.typed::<ByteArrayType>()
                .write_batch(&vals, Some(&defs), None)?;
        }
        Values::Fixed(_, v) => {
            let (vals, defs) = split(&v[start..end]);
            let vals: Vec<FixedLenByteArray> = vals
                .into_iter()
                .map(|b| FixedLenByteArray::from(ByteArray::from(b)))
                .collect();
            out.typed::<FixedLenByteArrayType>()
                .write_batch(&vals, Some(&defs), None)?;
        }
    }
    Ok(())
}

/// How [`rewrite_footer`] changes a footer.
#[derive(Clone, Copy, Debug, Default)]
pub struct Rewrite {
    /// The least and greatest values of the old rule: the deprecated
    /// fields `min` and `max` alone, and no column orders.
    pub old_rule: bool,
    /// No statistics of encodings.
    pub drop_encoding_stats: bool,
    /// No counts of `NULL` values.
    pub drop_null_counts: bool,

    /// The codec the footer names for every chunk of the column of this
    /// leaf, its pages left as they are.
    pub codec: Option<(usize, parquet::basic::Compression)>,
    /// The count of rows the footer gives the first group.
    pub first_group_rows: Option<i64>,
}

/// Rewrites the footer of the file at `path` as `rewrite` says, its pages
/// left as they are.
///
/// # Errors
///
/// `58030` for a file that cannot be read or written, `XX001` for one the
/// format cannot decode.
pub fn rewrite_footer(path: &Path, rewrite: Rewrite) -> Result<()> {
    let bytes = fs::read(path).map_err(|e| error::io(&e, "read test file"))?;
    let mut options = ParquetMetaDataOptions::new();
    // The whole statistics of encodings, which the writer writes back; a
    // mask it would drop.
    options.set_encoding_stats_as_mask(false);
    let file = fs::File::open(path).map_err(|e| error::io(&e, "open test file"))?;
    let metadata = ParquetMetaDataReader::new()
        .with_metadata_options(Some(options))
        .parse_and_finish(&file)
        .map_err(|e| error::damaged(&e))?;
    let footer_len = footer_length(&bytes)?;
    let data_end = bytes.len() - 8 - footer_len;

    let file_meta = metadata.file_metadata();
    let schema = file_meta.schema_descr_ptr();
    let mut groups = Vec::new();
    for group in metadata.row_groups() {
        let mut columns = Vec::new();
        for (leaf, column) in group.columns().iter().enumerate() {
            let mut builder = column.clone().into_builder();
            if let Some((at, codec)) = rewrite.codec
                && at == leaf
            {
                builder = builder.set_compression(codec);
            }
            if rewrite.drop_encoding_stats {
                builder = builder.clear_page_encoding_stats();
            }
            if let Some(stats) = column.statistics() {
                let changed = restate(stats, rewrite);
                builder = match changed {
                    Some(stats) => builder.set_statistics(stats),
                    None => builder.clear_statistics(),
                };
            }
            columns.push(builder.build().map_err(|e| error::damaged(&e))?);
        }
        let mut builder = group.clone().into_builder().set_column_metadata(columns);
        if let Some(rows) = rewrite.first_group_rows
            && groups.is_empty()
        {
            builder = builder.set_num_rows(rows);
        }
        let rebuilt: RowGroupMetaData = builder.build().map_err(|e| error::damaged(&e))?;
        groups.push(rebuilt);
    }
    let file_meta = FileMetaData::new(
        file_meta.version(),
        file_meta.num_rows(),
        file_meta.created_by().map(str::to_owned),
        file_meta.key_value_metadata().cloned(),
        Arc::clone(&schema),
        if rewrite.old_rule {
            None
        } else {
            file_meta.column_orders().cloned()
        },
    );
    let rewritten = ParquetMetaData::new(file_meta, groups);
    let mut out = bytes[..data_end].to_vec();
    ParquetMetaDataWriter::new(&mut out, &rewritten)
        .finish()
        .map_err(|e| error::damaged(&e))?;
    fs::write(path, out).map_err(|e| error::io(&e, "write test file"))
}

fn footer_length(bytes: &[u8]) -> Result<usize> {
    let tail = bytes
        .len()
        .checked_sub(8)
        .and_then(|start| bytes.get(start..start + 4))
        .ok_or_else(|| Error::new(error::DATA_CORRUPTED, "a file shorter than its tail"))?;
    let mut word = [0u8; 4];
    word.copy_from_slice(tail);
    usize::try_from(u32::from_le_bytes(word))
        .ok()
        .filter(|&len| len + 8 <= bytes.len())
        .ok_or_else(|| Error::new(error::DATA_CORRUPTED, "a footer longer than its file"))
}

/// The statistics `stats` as `rewrite` restates them, `None` for none.
fn restate(stats: &Statistics, rewrite: Rewrite) -> Option<Statistics> {
    fn one<T: parquet::data_type::AsBytes + Clone>(
        s: &ValueStatistics<T>,
        rewrite: Rewrite,
        deprecated: bool,
        wrap: fn(ValueStatistics<T>) -> Statistics,
    ) -> Statistics
    where
        ValueStatistics<T>: Clone,
    {
        let nulls = if rewrite.drop_null_counts {
            None
        } else {
            s.null_count_opt()
        };
        let restated = ValueStatistics::new(
            s.min_opt().cloned(),
            s.max_opt().cloned(),
            s.distinct_count(),
            nulls,
            rewrite.old_rule || deprecated,
        )
        .with_backwards_compatible_min_max(rewrite.old_rule);
        wrap(restated)
    }
    let deprecated = stats.is_min_max_deprecated();
    Some(match stats {
        Statistics::Boolean(s) => one(s, rewrite, deprecated, Statistics::Boolean),
        Statistics::Int32(s) => one(s, rewrite, deprecated, Statistics::Int32),
        Statistics::Int64(s) => one(s, rewrite, deprecated, Statistics::Int64),
        Statistics::Int96(s) => one(s, rewrite, deprecated, Statistics::Int96),
        Statistics::Float(s) => one(s, rewrite, deprecated, Statistics::Float),
        Statistics::Double(s) => one(s, rewrite, deprecated, Statistics::Double),
        Statistics::ByteArray(s) => one(s, rewrite, deprecated, Statistics::ByteArray),
        Statistics::FixedLenByteArray(s) => {
            one(s, rewrite, deprecated, Statistics::FixedLenByteArray)
        }
    })
}

/// Heads the dictionary page of every chunk of the file at `path`
/// `PLAIN_DICTIONARY` instead of `PLAIN`, as writers of the format's first
/// version do; the values stay plain, the same bytes.
///
/// # Errors
///
/// `58030` for a file that cannot be read or written, `XX001` for one the
/// format cannot decode or whose dictionary page is not headed `PLAIN`.
pub fn head_dictionaries_plain_dictionary(path: &Path) -> Result<()> {
    let mut bytes = fs::read(path).map_err(|e| error::io(&e, "read test file"))?;
    let file = fs::File::open(path).map_err(|e| error::io(&e, "open test file"))?;
    let metadata = ParquetMetaDataReader::new()
        .parse_and_finish(&file)
        .map_err(|e| error::damaged(&e))?;
    for group in metadata.row_groups() {
        for column in group.columns() {
            let Some(offset) = column.dictionary_page_offset() else {
                continue;
            };
            let place = usize::try_from(offset)
                .ok()
                .and_then(|start| encoding_of_dictionary_header(&bytes, start))
                .ok_or_else(|| Error::new(error::DATA_CORRUPTED, "no dictionary page header"))?;
            // A zigzag varint of the encoding: PLAIN, 0, is the byte 0;
            // PLAIN_DICTIONARY, 2, the byte 4, of the same length.
            if bytes[place] != zigzag(Encoding::PLAIN) {
                return Err(Error::new(
                    error::DATA_CORRUPTED,
                    "a dictionary page not headed PLAIN",
                ));
            }
            bytes[place] = zigzag(Encoding::PLAIN_DICTIONARY);
        }
    }
    fs::write(path, bytes).map_err(|e| error::io(&e, "write test file"))
}

fn zigzag(encoding: Encoding) -> u8 {
    match encoding {
        Encoding::PLAIN => 0,
        Encoding::PLAIN_DICTIONARY => 4,
        Encoding::RLE_DICTIONARY => 16,
        _ => 0xff,
    }
}

/// Heads the first data page of the column of leaf `leaf` in group `group`
/// of the file at `path` `RLE_DICTIONARY` instead of `PLAIN`, its values
/// left plain and no dictionary page: bytes the crate's decoders cannot
/// read, the time of the file kept.
///
/// # Errors
///
/// `58030` for a file that cannot be read or written, `XX001` for one the
/// format cannot decode or whose data page is not headed `PLAIN`.
pub fn head_data_page_rle_dictionary(path: &Path, group: usize, leaf: usize) -> Result<()> {
    let mut bytes = fs::read(path).map_err(|e| error::io(&e, "read test file"))?;
    let file = fs::File::open(path).map_err(|e| error::io(&e, "open test file"))?;
    let modified = file
        .metadata()
        .and_then(|m| m.modified())
        .map_err(|e| error::io(&e, "stat test file"))?;
    let metadata = ParquetMetaDataReader::new()
        .parse_and_finish(&file)
        .map_err(|e| error::damaged(&e))?;
    let chunk = metadata.row_group(group).column(leaf);
    let place = usize::try_from(chunk.data_page_offset())
        .ok()
        .and_then(|start| encoding_in_header(&bytes, start, 5))
        .ok_or_else(|| Error::new(error::DATA_CORRUPTED, "no data page header"))?;
    if bytes[place] != zigzag(Encoding::PLAIN) {
        return Err(Error::new(
            error::DATA_CORRUPTED,
            "a data page not headed PLAIN",
        ));
    }
    bytes[place] = zigzag(Encoding::RLE_DICTIONARY);
    fs::write(path, bytes).map_err(|e| error::io(&e, "write test file"))?;
    fs::File::options()
        .write(true)
        .open(path)
        .and_then(|f| f.set_modified(modified))
        .map_err(|e| error::io(&e, "keep the time of test file"))
}

/// The place of the byte that holds the encoding of the dictionary page
/// header that begins at `start`.
fn encoding_of_dictionary_header(bytes: &[u8], start: usize) -> Option<usize> {
    encoding_in_header(bytes, start, 7)
}

/// The place of the byte that holds the encoding of the page header that
/// begins at `start`: field 2, an `i32`, of the struct of field `which`
/// of `PageHeader` (5 of a data page, 7 of a dictionary page), in
/// Thrift's compact protocol.
fn encoding_in_header(bytes: &[u8], start: usize, which: i16) -> Option<usize> {
    let mut at = start;
    let mut last = 0i16;
    loop {
        let head = *bytes.get(at)?;
        at += 1;
        if head == 0 {
            return None;
        }
        let kind = head & 0x0f;
        let delta = i16::from(head >> 4);
        let field = if delta == 0 {
            let (value, len) = varint(bytes.get(at..)?)?;
            at += len;
            unzig(value) as i16
        } else {
            last + delta
        };
        last = field;
        if field == which && kind == 12 {
            // The page's own header: field 1, num_values, then 2.
            let mut inner = 0i16;
            loop {
                let head = *bytes.get(at)?;
                at += 1;
                let kind = head & 0x0f;
                let delta = i16::from(head >> 4);
                if head == 0 || delta == 0 {
                    return None;
                }
                inner += delta;
                if inner == 2 && kind == 5 {
                    return Some(at);
                }
                at += skip(bytes.get(at..)?, kind)?;
            }
        }
        at += skip(bytes.get(at..)?, kind)?;
    }
}

fn varint(bytes: &[u8]) -> Option<(u64, usize)> {
    let mut value = 0u64;
    for (i, &byte) in bytes.iter().enumerate().take(10) {
        value |= u64::from(byte & 0x7f) << (7 * i);
        if byte & 0x80 == 0 {
            return Some((value, i + 1));
        }
    }
    None
}

fn unzig(value: u64) -> i64 {
    ((value >> 1) as i64) ^ -((value & 1) as i64)
}

/// The bytes of a value of compact type `kind` at the start of `bytes`:
/// the types a page header's fields before the dictionary header use.
fn skip(bytes: &[u8], kind: u8) -> Option<usize> {
    match kind {
        1 | 2 => Some(0),
        3 => Some(1),
        4..=6 => varint(bytes).map(|(_, len)| len),
        8 => varint(bytes).map(|(len, n)| n + len as usize),
        12 => {
            let mut at = 0;
            loop {
                let head = *bytes.get(at)?;
                at += 1;
                if head == 0 {
                    return Some(at);
                }
                if head >> 4 == 0 {
                    at += varint(bytes.get(at..)?)?.1;
                }
                at += skip(bytes.get(at..)?, head & 0x0f)?;
            }
        }
        _ => None,
    }
}
