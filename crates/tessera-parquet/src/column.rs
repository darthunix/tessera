//! A column of a foreign table mapped to a column of a Parquet file: the
//! file column found by its name, its physical type and mark checked
//! against the wrapper's list, the read plan of `datum-arrow` its values
//! cross, and the reader's view of the footer for it.

use std::sync::Arc;

use arrow_schema::{DataType, Field, Schema};
use datum_arrow::{PgType, ReadOptions, ReadPlan, TextBytes, Unit};
use parquet::arrow::arrow_reader::{ArrowReaderMetadata, ArrowReaderOptions};
use parquet::basic::{
    Compression, ConvertedType, LogicalType, Repetition, TimeUnit as ParquetUnit, Type as Physical,
};
use parquet::file::metadata::{FileMetaData, ParquetMetaData, RowGroupMetaData};
use parquet::schema::types::{ColumnDescriptor, SchemaDescriptor, Type};

use crate::error::{
    self, Error, FEATURE_NOT_SUPPORTED, INVALID_PARAMETER_VALUE, Result, UNDEFINED_COLUMN,
};
use crate::file::ParquetFile;

/// A file column's physical type and mark, as the wrapper's list takes
/// them: what the least and greatest values and a dictionary page of the
/// column hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileType {
    /// `BOOLEAN`.
    Boolean,
    /// `INT32` or `INT64` of `bits`, signed or not.
    Int {
        /// The width of the values, 8, 16, 32 or 64.
        bits: u8,
        /// Whether the values are signed.
        signed: bool,
    },
    /// `INT32` marked `DATE`: days since 1970-01-01.
    Date,
    /// `INT64` marked `TIMESTAMP` in milliseconds or microseconds.
    Timestamp {
        /// Microseconds where true, milliseconds where false.
        micros: bool,
        /// Whether the time is adjusted to UTC.
        utc: bool,
    },
    /// `FLOAT`.
    Float,
    /// `DOUBLE`.
    Double,
    /// `BYTE_ARRAY` unmarked or marked as a string: bytes either way.
    Bytes,
    /// `FIXED_LEN_BYTE_ARRAY` unmarked, of its length.
    Fixed(usize),
}

/// A column of a foreign table, as the wrapper gives it: the file
/// column's name, the declared type, and the column's options.
#[derive(Clone, Copy, Debug)]
pub struct ColumnRequest<'a> {
    /// The file column's name, compared exactly.
    pub name: &'a str,
    /// The declared type's object identifier.
    pub type_oid: u32,
    /// The declared type's modifier, -1 for none.
    pub typmod: i32,
    /// The option `unit`, where the column has it.
    pub unit: Option<Unit>,
    /// The option `utf8`, where the column has it.
    pub utf8: Option<TextBytes>,
    /// Whether the database's encoding is UTF-8.
    pub server_utf8: bool,
}

/// A column of a foreign table mapped to a column of a file.
#[derive(Clone, Debug)]
pub struct MappedColumn {
    leaf: usize,
    file_type: FileType,
    nullable: bool,
    plan: ReadPlan,
    view: ArrowReaderMetadata,
    dictionary_view: Option<(ArrowReaderMetadata, ReadPlan)>,
}

impl MappedColumn {
    /// The file column's place among the file's leaves.
    pub fn leaf(&self) -> usize {
        self.leaf
    }

    /// The file column's type.
    pub fn file_type(&self) -> FileType {
        self.file_type
    }

    /// Whether the file column may hold `NULL`.
    pub fn nullable(&self) -> bool {
        self.nullable
    }

    /// The bytes of the column's chunks in the file, as stored,
    /// compressed: what a scan of the column reads.
    pub fn chunk_bytes(&self) -> u64 {
        self.view
            .metadata()
            .row_groups()
            .iter()
            .map(|group| u64::try_from(group.column(0).compressed_size()).unwrap_or(0))
            .sum()
    }

    /// The read plan of the column's values, as the reader gives them in a
    /// plain array.
    pub fn plan(&self) -> &ReadPlan {
        &self.plan
    }

    /// The reader's view of the footer for the column, and the plan of
    /// what it reads: a chunk of bytes with a dictionary page read as a
    /// dictionary array where `dictionary` says.
    pub(crate) fn view(&self, dictionary: bool) -> (&ArrowReaderMetadata, &ReadPlan) {
        match (&self.dictionary_view, dictionary) {
            (Some((view, plan)), true) => (view, plan),
            _ => (&self.view, &self.plan),
        }
    }
}

/// Maps `request` to a column of `file`.
///
/// # Errors
///
/// In this order: `42703` for a column the file lacks; `0A000` for a file
/// column of a type outside the wrapper's list, repeated, nested or a
/// group, or of chunks compressed other than by Snappy or not at all;
/// `0A000` for a declared type the wrapper does not take; `22023` for a
/// `unit` or `utf8` on a declared type it does not apply to; then the
/// errors of [`ReadPlan::new`]: `22023` for a `unit` on a file column it
/// does not apply to, `42804` for a pair the plan does not take, `54000`
/// for fixed-length bytes larger than PostgreSQL holds and `0A000` for
/// text in a database whose encoding is not UTF-8.
pub fn map_column(file: &ParquetFile, request: &ColumnRequest<'_>) -> Result<MappedColumn> {
    let metadata = file.metadata();
    let schema = metadata.file_metadata().schema_descr();
    let (leaf, field) = find(schema, request.name)?;
    let descr = schema.column(leaf);
    let file_type = classify(&descr).ok_or_else(|| outside(request.name, field))?;
    for group in metadata.row_groups() {
        let codec = group.column(leaf).compression();
        if !matches!(codec, Compression::UNCOMPRESSED | Compression::SNAPPY) {
            return Err(Error::new(
                FEATURE_NOT_SUPPORTED,
                format!(
                    "Parquet column \"{}\" is compressed by {codec}, which is not supported",
                    request.name
                ),
            ));
        }
    }
    let pg = PgType::new(request.type_oid, request.typmod)?;
    let text_type = matches!(pg, PgType::Text | PgType::Varchar(_));
    let time_type = matches!(pg, PgType::Date | PgType::Timestamp | PgType::Timestamptz);
    if request.utf8.is_some() && !text_type {
        return Err(not_applicable("utf8", request.name));
    }
    if request.unit.is_some() && !time_type {
        return Err(not_applicable("unit", request.name));
    }
    let options = ReadOptions {
        unit: request.unit,
        // `utf8` is the declared type's, and bytes alone are checked: a
        // text column over another physical type is a pair refused.
        text: (text_type && file_type == FileType::Bytes)
            .then_some(request.utf8.unwrap_or(TextBytes::Checked)),
        server_utf8: request.server_utf8,
    };
    let nullable = descr.self_type().get_basic_info().repetition() == Repetition::OPTIONAL;
    let view = view_of(metadata, leaf, &descr, file_type, None)?;
    let arrow = view
        .schema()
        .fields()
        .first()
        .map(|f| f.data_type().clone())
        .ok_or_else(|| Error::new(error::INTERNAL_ERROR, "a view without its column"))?;
    let plan = ReadPlan::new(&arrow, pg, options)?;
    let dictionary_view = if file_type == FileType::Bytes {
        let dictionary =
            DataType::Dictionary(Box::new(DataType::Int32), Box::new(DataType::LargeBinary));
        let view = view_of(metadata, leaf, &descr, file_type, Some(dictionary.clone()))?;
        let plan = ReadPlan::new(&dictionary, pg, options)?;
        Some((view, plan))
    } else {
        None
    };
    Ok(MappedColumn {
        leaf,
        file_type,
        nullable,
        plan,
        view,
        dictionary_view,
    })
}

/// The leaf and the top-level field of the column named `name`.
fn find<'s>(schema: &'s SchemaDescriptor, name: &str) -> Result<(usize, &'s Type)> {
    let field = schema
        .root_schema()
        .get_fields()
        .iter()
        .find(|f| f.name() == name)
        .ok_or_else(|| {
            Error::new(
                UNDEFINED_COLUMN,
                format!("Parquet file has no column \"{name}\""),
            )
        })?;
    if !field.is_primitive() {
        return Err(outside(name, field));
    }
    let leaf = (0..schema.num_columns())
        .find(|&i| {
            let column = schema.column(i);
            let parts = column.path().parts();
            parts.len() == 1 && parts[0] == name
        })
        .ok_or_else(|| Error::new(error::INTERNAL_ERROR, "a field without its leaf"))?;
    Ok((leaf, field))
}

fn outside(name: &str, field: &Type) -> Error {
    let info = field.get_basic_info();
    let mark = match (info.logical_type_ref(), info.converted_type()) {
        (Some(logical), _) => format!("{logical:?}"),
        (None, ConvertedType::NONE) => String::from("unmarked"),
        (None, converted) => converted.to_string(),
    };
    let what = if field.is_primitive() {
        format!("{} {mark}", field.get_physical_type())
    } else {
        format!("a group, {mark}")
    };
    Error::new(
        FEATURE_NOT_SUPPORTED,
        format!("Parquet column \"{name}\" of type {what} is not supported"),
    )
}

fn not_applicable(option: &str, name: &str) -> Error {
    Error::new(
        INVALID_PARAMETER_VALUE,
        format!("option \"{option}\" does not apply to the type of column \"{name}\""),
    )
}

/// The wrapper's type of a file column, `None` for one outside its list:
/// the logical type, where the file gives one, deciding over the older
/// converted type.
fn classify(descr: &ColumnDescriptor) -> Option<FileType> {
    // A top-level field alone is looked up, so a repeated one is the only
    // column of more than one value a row.
    if descr.self_type().get_basic_info().repetition() == Repetition::REPEATED {
        return None;
    }
    let logical = descr.logical_type_ref();
    let converted = descr.converted_type();
    let unmarked = logical.is_none() && converted == ConvertedType::NONE;
    let int = |bits, signed| Some(FileType::Int { bits, signed });
    match descr.physical_type() {
        Physical::BOOLEAN => unmarked.then_some(FileType::Boolean),
        Physical::INT32 => match (logical, converted) {
            (
                Some(LogicalType::Integer {
                    bit_width,
                    is_signed,
                }),
                _,
            ) => match *bit_width {
                8 => int(8, *is_signed),
                16 => int(16, *is_signed),
                32 => int(32, *is_signed),
                _ => None,
            },
            (Some(LogicalType::Date), _) => Some(FileType::Date),
            (Some(_), _) => None,
            (None, ConvertedType::NONE | ConvertedType::INT_32) => int(32, true),
            (None, ConvertedType::INT_8) => int(8, true),
            (None, ConvertedType::INT_16) => int(16, true),
            (None, ConvertedType::UINT_8) => int(8, false),
            (None, ConvertedType::UINT_16) => int(16, false),
            (None, ConvertedType::UINT_32) => int(32, false),
            (None, ConvertedType::DATE) => Some(FileType::Date),
            (None, _) => None,
        },
        Physical::INT64 => match (logical, converted) {
            (
                Some(LogicalType::Integer {
                    bit_width: 64,
                    is_signed: true,
                }),
                _,
            ) => int(64, true),
            (
                Some(LogicalType::Timestamp {
                    is_adjusted_to_u_t_c,
                    unit,
                }),
                _,
            ) => match unit {
                ParquetUnit::MILLIS => Some(FileType::Timestamp {
                    micros: false,
                    utc: *is_adjusted_to_u_t_c,
                }),
                ParquetUnit::MICROS => Some(FileType::Timestamp {
                    micros: true,
                    utc: *is_adjusted_to_u_t_c,
                }),
                ParquetUnit::NANOS => None,
            },
            (Some(_), _) => None,
            (None, ConvertedType::NONE | ConvertedType::INT_64) => int(64, true),
            // The older marks, which the format reads as adjusted to UTC.
            (None, ConvertedType::TIMESTAMP_MILLIS) => Some(FileType::Timestamp {
                micros: false,
                utc: true,
            }),
            (None, ConvertedType::TIMESTAMP_MICROS) => Some(FileType::Timestamp {
                micros: true,
                utc: true,
            }),
            (None, _) => None,
        },
        Physical::FLOAT => unmarked.then_some(FileType::Float),
        Physical::DOUBLE => unmarked.then_some(FileType::Double),
        Physical::BYTE_ARRAY => match (logical, converted) {
            (None | Some(LogicalType::String), ConvertedType::NONE | ConvertedType::UTF8) => {
                Some(FileType::Bytes)
            }
            _ => None,
        },
        Physical::FIXED_LEN_BYTE_ARRAY => {
            if unmarked {
                usize::try_from(descr.type_length())
                    .ok()
                    .map(FileType::Fixed)
            } else {
                None
            }
        }
        Physical::INT96 => None,
    }
}

/// The reader's view of the footer for the column at `leaf`: a schema of
/// that column alone, its marks taken off where it is read as bytes, so
/// that the crate checks no UTF-8; no Arrow schema of the file's metadata;
/// and each group's chunk of that column alone. `arrow`, where given, is
/// the Arrow type the reader asks for.
fn view_of(
    metadata: &ParquetMetaData,
    leaf: usize,
    descr: &ColumnDescriptor,
    file_type: FileType,
    arrow: Option<DataType>,
) -> Result<ArrowReaderMetadata> {
    let original = descr.self_type();
    let info = original.get_basic_info();
    let column = if file_type == FileType::Bytes {
        Type::primitive_type_builder(info.name(), Physical::BYTE_ARRAY)
            .with_repetition(info.repetition())
            .build()
            .map_err(|e| error::damaged(&e))?
    } else {
        original.clone()
    };
    let nullable = info.repetition() == Repetition::OPTIONAL;
    let root = Type::group_type_builder("schema")
        .with_fields(vec![Arc::new(column)])
        .build()
        .map_err(|e| error::damaged(&e))?;
    let schema = Arc::new(SchemaDescriptor::new(Arc::new(root)));
    let file_meta = metadata.file_metadata();
    let orders = file_meta
        .column_orders()
        .map(|_| vec![file_meta.column_order(leaf)]);
    let one = FileMetaData::new(
        file_meta.version(),
        file_meta.num_rows(),
        file_meta.created_by().map(str::to_owned),
        None,
        Arc::clone(&schema),
        orders,
    );
    let groups = metadata
        .row_groups()
        .iter()
        .map(|group| {
            let mut builder = RowGroupMetaData::builder(Arc::clone(&schema))
                .set_num_rows(group.num_rows())
                .set_total_byte_size(group.column(leaf).uncompressed_size())
                .set_column_metadata(vec![group.column(leaf).clone()]);
            if let Some(ordinal) = group.ordinal() {
                builder = builder.set_ordinal(ordinal);
            }
            builder.build().map_err(|e| error::damaged(&e))
        })
        .collect::<Result<Vec<_>>>()?;
    let view = Arc::new(ParquetMetaData::new(one, groups));
    let mut options = ArrowReaderOptions::new().with_skip_arrow_metadata(true);
    let asked = arrow.or((file_type == FileType::Bytes).then_some(DataType::LargeBinary));
    if let Some(asked) = asked {
        let schema = Schema::new(vec![Field::new(info.name(), asked, nullable)]);
        options = options.with_schema(Arc::new(schema));
    }
    ArrowReaderMetadata::try_new(view, options).map_err(|e| error::damaged(&e))
}

#[cfg(test)]
mod tests;
