//! The reader of Parquet files under Tessera's Parquet foreign data
//! wrapper, `tessera_parquet`.
//!
//! The wrapper's C code calls it through the entry points of [`c`],
//! declared in `parquet/tessera_parquet_reader.h`; the Rust items below
//! are what those entry points are made of, and what the tests use.
//!
//! - [`ParquetFile`]: a file opened, its footer read, and what `fstat`
//!   said of it, so that a scan reads one version of a file.
//! - [`MappedColumn`]: a column of a foreign table mapped to a column of
//!   the file, with the read plan of `datum-arrow` its values cross.
//! - [`ColumnReader`]: a column of a group of rows read window by window
//!   for a selection of rows, forward.
//! - [`ClauseTest`]: a clause the source checks, made once from the
//!   clause's values and a column's read plan, applied to the arrays a
//!   reader gives before any value is made.
//! - [`GroupStats`] and [`Dictionary`]: what a footer and a dictionary
//!   page say of a column's chunk, for the skipping of groups of rows.

mod bytes;
pub mod c;
mod clause;
mod column;
mod dictionary;
mod error;
mod file;
mod pages;
mod reader;
pub mod snappy;
mod stats;

#[cfg(any(test, feature = "test-files"))]
pub mod testing;

pub use clause::{ClauseKind, ClauseTest};
pub use column::{ColumnRequest, FileType, MappedColumn, map_column};
pub use dictionary::{Dictionary, DictionaryBytes, read_dictionary};
pub use error::{Error, Result};
pub use file::{Identity, ParquetFile};
pub use reader::{ColumnReader, Window, input_of};
pub use stats::{Bounds, Empties, GroupStats, group_stats};
