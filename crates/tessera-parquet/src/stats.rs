//! What a footer says of a column's chunk in a group of rows, under
//! Parquet's rules of trust: its count of `NULL` values, its least and
//! greatest values, the emptiness of its least and greatest strings, and
//! whether every data page is encoded through its dictionary.

use parquet::basic::{ColumnOrder, Encoding};
use parquet::file::metadata::ParquetMetaData;
use parquet::file::statistics::Statistics;

use crate::column::{FileType, MappedColumn};
use crate::file::ParquetFile;

/// The least and greatest stored values of a chunk, in the stored unit of
/// the column's read plan: the integer, the day since 1970, or the
/// millisecond or microsecond since 1970.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bounds {
    /// The least stored value.
    pub least: i64,
    /// The greatest stored value.
    pub greatest: i64,
}

/// The emptiness of the least and greatest strings of a chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Empties {
    /// Whether the least value is the empty string: where it is not, no
    /// row is, since a least value a writer cut short is a prefix of the
    /// least row.
    pub least_empty: bool,
    /// Whether the greatest value is the empty string: where it is, every
    /// row that is not `NULL` is.
    pub greatest_empty: bool,
}

/// What a footer says of a column's chunk in one group of rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct GroupStats {
    /// The count of `NULL` values, where the footer gives it.
    pub nulls: Option<u64>,
    /// The least and greatest values of integers, dates and times, where
    /// the footer gives them and the format lets a reader trust them.
    pub bounds: Option<Bounds>,
    /// For strings, the emptiness of the least and greatest values, where
    /// the footer gives them and the format lets a reader trust them.
    pub empties: Option<Empties>,
    /// Whether the chunk has a dictionary page and every data page is
    /// encoded through it, by the footer's statistics of encodings.
    pub dictionary_only: bool,
}

/// What the footer of `file` says of `column`'s chunk in group `group`;
/// nothing for a group past the file's.
///
/// The least and greatest values of integers, dates and times are taken
/// from the fields `min_value` and `max_value` only where the file's
/// column orders give the column the order of its type, and from the
/// older fields `min` and `max`, which a writer always sorted as signed,
/// only for signed values, where the footer has no newer ones; a bound
/// of a value outside its mark's width gives none, since the reader
/// narrows such a row. The emptiness of strings is taken from the newer
/// fields alone, under the same column orders: old writers wrote wrong
/// least and greatest values of bytes.
pub fn group_stats(file: &ParquetFile, column: &MappedColumn, group: usize) -> GroupStats {
    stats_of(file.metadata(), column, group)
}

fn stats_of(metadata: &ParquetMetaData, column: &MappedColumn, group: usize) -> GroupStats {
    let Some(group) = metadata.row_groups().get(group) else {
        return GroupStats::default();
    };
    let chunk = group.column(column.leaf());
    // The mask holds the encodings of the data pages alone: each through
    // the dictionary, by either of its names.
    let dictionary_only = chunk.dictionary_page_offset().is_some()
        && chunk.page_encoding_stats_mask().is_some_and(|mask| {
            mask.encodings().next().is_some()
                && mask
                    .encodings()
                    .all(|e| matches!(e, Encoding::PLAIN_DICTIONARY | Encoding::RLE_DICTIONARY))
        });
    let Some(stats) = chunk.statistics() else {
        return GroupStats {
            dictionary_only,
            ..GroupStats::default()
        };
    };
    let ordered = matches!(
        metadata.file_metadata().column_order(column.leaf()),
        ColumnOrder::TYPE_DEFINED_ORDER(_)
    );
    let trusted = if stats.is_min_max_deprecated() {
        signed(column.file_type())
    } else {
        ordered
    };
    GroupStats {
        nulls: stats.null_count_opt(),
        bounds: if trusted {
            bounds(stats, column.file_type())
        } else {
            None
        },
        empties: match stats {
            // Of `BYTE_ARRAY` alone, which the wrapper reads as bytes.
            Statistics::ByteArray(s) if ordered && !stats.is_min_max_deprecated() => {
                match (s.min_opt(), s.max_opt()) {
                    (Some(least), Some(greatest)) => Some(Empties {
                        least_empty: least.data().is_empty(),
                        greatest_empty: greatest.data().is_empty(),
                    }),
                    _ => None,
                }
            }
            _ => None,
        },
        dictionary_only,
    }
}

/// Whether a writer's signed order is the order of the values of a type.
fn signed(file_type: FileType) -> bool {
    matches!(
        file_type,
        FileType::Int { signed: true, .. } | FileType::Date | FileType::Timestamp { .. }
    )
}

fn bounds(stats: &Statistics, file_type: FileType) -> Option<Bounds> {
    let pair = |least: Option<i64>, greatest: Option<i64>| {
        Some(Bounds {
            least: least?,
            greatest: greatest?,
        })
    };
    let found = match (stats, file_type) {
        (Statistics::Int32(s), FileType::Int { signed: false, .. }) => pair(
            s.min_opt().map(|&v| i64::from(v as u32)),
            s.max_opt().map(|&v| i64::from(v as u32)),
        ),
        (Statistics::Int32(s), FileType::Int { .. } | FileType::Date) => pair(
            s.min_opt().map(|&v| i64::from(v)),
            s.max_opt().map(|&v| i64::from(v)),
        ),
        (Statistics::Int64(s), FileType::Int { signed: true, .. } | FileType::Timestamp { .. }) => {
            pair(s.min_opt().copied(), s.max_opt().copied())
        }
        _ => None,
    }?;
    let width = match file_type {
        FileType::Int {
            bits: 8,
            signed: true,
        } => Some((i64::from(i8::MIN), i64::from(i8::MAX))),
        FileType::Int {
            bits: 8,
            signed: false,
        } => Some((0, i64::from(u8::MAX))),
        FileType::Int {
            bits: 16,
            signed: true,
        } => Some((i64::from(i16::MIN), i64::from(i16::MAX))),
        FileType::Int {
            bits: 16,
            signed: false,
        } => Some((0, i64::from(u16::MAX))),
        _ => None,
    };
    match width {
        Some((low, high)) if found.least < low || found.greatest > high => None,
        _ => Some(found),
    }
}

#[cfg(test)]
mod tests;
