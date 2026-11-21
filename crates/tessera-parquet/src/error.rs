//! The errors of the reader: each with the SQLSTATE the wrapper raises for
//! it, a message for people and, for a value that failed, its row.

use std::fmt;

/// The result of a call of the reader.
pub type Result<T, E = Error> = core::result::Result<T, E>;

/// An error of the reader, with the SQLSTATE PostgreSQL raises for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    sqlstate: [u8; 5],
    message: String,
    row: Option<usize>,
}

/// `58P01`, `undefined_file`: a file that does not exist.
pub const UNDEFINED_FILE: &str = "58P01";
/// `58030`, `io_error`: a file that cannot be opened or read.
pub const IO_ERROR: &str = "58030";
/// `XX001`, `data_corrupted`: bytes the format cannot decode.
pub const DATA_CORRUPTED: &str = "XX001";
/// `0A000`, `feature_not_supported`: a type, a codec or a shape of a
/// file the wrapper does not read.
pub const FEATURE_NOT_SUPPORTED: &str = "0A000";
/// `42703`, `undefined_column`: a column the file lacks.
pub const UNDEFINED_COLUMN: &str = "42703";
/// `55000`, `object_not_in_prerequisite_state`: a file that changed
/// under a scan.
pub const OBJECT_NOT_IN_PREREQUISITE_STATE: &str = "55000";
/// `22023`, `invalid_parameter_value`: an option where it does not
/// apply.
pub const INVALID_PARAMETER_VALUE: &str = "22023";
/// `XX000`, `internal_error`: a call of a broken shape, or a panic.
pub const INTERNAL_ERROR: &str = "XX000";

impl Error {
    /// An error of `sqlstate`, five characters of PostgreSQL's codes.
    pub(crate) fn new(sqlstate: &str, message: impl Into<String>) -> Self {
        let mut code = [b'X'; 5];
        for (place, byte) in code.iter_mut().zip(sqlstate.bytes()) {
            *place = byte;
        }
        Error {
            sqlstate: code,
            message: message.into(),
            row: None,
        }
    }

    /// The same error, of the value of `row`.
    pub(crate) fn at(mut self, row: usize) -> Self {
        self.row = Some(row);
        self
    }

    /// The five characters of the SQLSTATE.
    pub fn sqlstate(&self) -> &str {
        // The code is built from a `&str` of ASCII characters alone.
        core::str::from_utf8(&self.sqlstate).unwrap_or("XX000")
    }

    /// The message, for people.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The row of the value that failed, where a value did.
    pub fn row(&self) -> Option<usize> {
        self.row
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.message, self.sqlstate())
    }
}

impl std::error::Error for Error {}

impl From<datum_arrow::Error> for Error {
    fn from(error: datum_arrow::Error) -> Self {
        let mapped = Error::new(error.sqlstate(), error.message());
        match error.slot() {
            Some(slot) => mapped.at(slot),
            None => mapped,
        }
    }
}

/// A file the format cannot decode, from the `parquet` crate's error.
pub(crate) fn damaged(error: &parquet::errors::ParquetError) -> Error {
    Error::new(
        DATA_CORRUPTED,
        format!("could not decode Parquet file: {error}"),
    )
}

/// A file that cannot be opened or read.
pub(crate) fn io(error: &std::io::Error, what: &str) -> Error {
    let code = if error.kind() == std::io::ErrorKind::NotFound {
        UNDEFINED_FILE
    } else {
        IO_ERROR
    };
    Error::new(code, format!("could not {what}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_error_keeps_its_code_and_row() {
        let error = Error::new(UNDEFINED_COLUMN, "no column").at(3);
        assert_eq!(
            (error.sqlstate(), error.message(), error.row()),
            ("42703", "no column", Some(3))
        );
        assert_eq!(error.to_string(), "no column (42703)");
    }

    #[test]
    fn an_error_of_datum_arrow_keeps_its_code_and_slot() {
        let plan = datum_arrow::ReadPlan::new(
            &arrow_schema::DataType::Utf8,
            datum_arrow::PgType::Int4,
            datum_arrow::ReadOptions {
                unit: None,
                text: None,
                server_utf8: true,
            },
        );
        let error = Error::from(plan.unwrap_err());
        assert_eq!(error.sqlstate(), "42804");
        assert_eq!(error.row(), None);
    }

    #[test]
    fn a_missing_file_and_another_failure_of_io() {
        let missing = std::io::Error::from(std::io::ErrorKind::NotFound);
        let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert_eq!(io(&missing, "open").sqlstate(), "58P01");
        assert_eq!(io(&denied, "open").sqlstate(), "58030");
    }
}
