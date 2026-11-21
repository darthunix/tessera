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

/// What a footer says of a column's chunk in one group of rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct GroupStats {
    /// The count of `NULL` values, where the footer gives it.
    pub nulls: Option<u64>,
    /// The least and greatest values of integers, dates and times, where
    /// the footer gives them and the format lets a reader trust them.
    pub bounds: Option<Bounds>,
    /// For strings, whether the least value is empty and whether the
    /// greatest is, where the footer gives them.
    pub empties: Option<(bool, bool)>,
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
/// only for signed values, where the footer has no newer ones. The
/// emptiness of strings is taken from either, since every order of bytes
/// puts the empty string first.
pub fn group_stats(file: &ParquetFile, column: &MappedColumn, group: usize) -> GroupStats {
    stats_of(file.metadata(), column, group)
}

fn stats_of(metadata: &ParquetMetaData, column: &MappedColumn, group: usize) -> GroupStats {
    let Some(group) = metadata.row_groups().get(group) else {
        return GroupStats::default();
    };
    let chunk = group.column(column.leaf());
    let dictionary_only = chunk.dictionary_page_offset().is_some()
        && chunk.page_encoding_stats_mask().is_some_and(|mask| {
            mask.is_only(Encoding::PLAIN_DICTIONARY) || mask.is_only(Encoding::RLE_DICTIONARY)
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
            Statistics::ByteArray(s) => match (s.min_opt(), s.max_opt()) {
                (Some(least), Some(greatest)) => {
                    Some((least.data().is_empty(), greatest.data().is_empty()))
                }
                _ => None,
            },
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
    match (stats, file_type) {
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
    }
}

#[cfg(test)]
mod tests;
