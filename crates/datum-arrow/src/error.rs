//! The errors of the crate's calls.
//!
//! A refusal of a plan, or a failure of a value, is an [`Error`]: its
//! [`ErrorKind`], which gives the SQLSTATE a PostgreSQL server raises for
//! it, the slot of the value that failed where a value did, and a message
//! for people. The error allocates nothing.

use core::fmt;

/// The result of the crate's calls.
pub type Result<T, E = Error> = core::result::Result<T, E>;

/// What kind of refusal or failure an [`Error`] is.
///
/// The kinds are few and closed, and each has the SQLSTATE that a
/// PostgreSQL server raises for it ([`ErrorKind::sqlstate`]), so that a
/// caller inside the server raises the error PostgreSQL would.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    /// A type, a type modifier, a plan or a server the crate does not
    /// support: `0A000`, `feature_not_supported`.
    NotSupported,
    /// A pair of types the crate does not convert: `42804`,
    /// `datatype_mismatch`.
    TypeMismatch,
    /// An option given where it does not apply: `22023`,
    /// `invalid_parameter_value`.
    InvalidOption,
    /// A value outside the range of its target type: `22008`,
    /// `datetime_field_overflow`. Only days and times can be out of
    /// range, since a plan of integers takes only an integer type that
    /// holds every value of its source.
    OutOfRange,
    /// Bytes that are not UTF-8, or a zero byte, in a text: `22021`,
    /// `character_not_in_repertoire`.
    InvalidText,
    /// A string longer than a `varchar`'s length: `22001`,
    /// `string_data_right_truncation`.
    TooLong,
    /// A value larger than PostgreSQL holds, or values written in one
    /// call past what Arrow's offsets hold: `54000`,
    /// `program_limit_exceeded`.
    TooLarge,
    /// A call whose buffers break the shape of its arrays or of the
    /// call: `XX000`, `internal_error`. It is the caller's bug, never a
    /// value's.
    BrokenShape,
}

impl ErrorKind {
    /// The SQLSTATE of the kind, the five characters a PostgreSQL server
    /// reports.
    pub const fn sqlstate(self) -> &'static str {
        match self {
            ErrorKind::NotSupported => "0A000",
            ErrorKind::TypeMismatch => "42804",
            ErrorKind::InvalidOption => "22023",
            ErrorKind::OutOfRange => "22008",
            ErrorKind::InvalidText => "22021",
            ErrorKind::TooLong => "22001",
            ErrorKind::TooLarge => "54000",
            ErrorKind::BrokenShape => "XX000",
        }
    }
}

/// A refusal of a plan, or a failure of a value.
///
/// It carries its [`ErrorKind`], the slot of the value that failed where
/// a value did, and a message for people that names the rule; a caller
/// that reports the error adds the column and the type it knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Error {
    kind: ErrorKind,
    slot: Option<usize>,
    message: &'static str,
}

impl Error {
    /// An error of `kind` with `message` and no slot.
    pub(crate) const fn new(kind: ErrorKind, message: &'static str) -> Self {
        Error {
            kind,
            slot: None,
            message,
        }
    }

    /// The same error, given the slot of the value that failed.
    #[allow(dead_code)] // the conversions of values report their slots
    pub(crate) const fn at(self, slot: usize) -> Self {
        Error {
            slot: Some(slot),
            ..self
        }
    }

    /// The kind of the error.
    pub const fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// The SQLSTATE of the error's kind.
    pub const fn sqlstate(&self) -> &'static str {
        self.kind.sqlstate()
    }

    /// The slot of the value that failed, counted from the first slot of
    /// the column the call was given; `None` for a refusal of a plan or
    /// of a call's shape.
    pub const fn slot(&self) -> Option<usize> {
        self.slot
    }

    /// The message, for people: the rule that was broken, in the words a
    /// PostgreSQL server would use where it has them.
    pub const fn message(&self) -> &'static str {
        self.message
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.slot {
            Some(slot) => write!(f, "{} (slot {slot})", self.message),
            None => f.write_str(self.message),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_has_its_sqlstate() {
        let kinds = [
            (ErrorKind::NotSupported, "0A000"),
            (ErrorKind::TypeMismatch, "42804"),
            (ErrorKind::InvalidOption, "22023"),
            (ErrorKind::OutOfRange, "22008"),
            (ErrorKind::InvalidText, "22021"),
            (ErrorKind::TooLong, "22001"),
            (ErrorKind::TooLarge, "54000"),
            (ErrorKind::BrokenShape, "XX000"),
        ];
        for (kind, sqlstate) in kinds {
            assert_eq!(kind.sqlstate(), sqlstate);
            assert_eq!(Error::new(kind, "m").sqlstate(), sqlstate);
        }
    }

    #[test]
    fn an_error_shows_its_slot() {
        let error = Error::new(ErrorKind::OutOfRange, "date out of range");
        assert_eq!(error.slot(), None);
        assert_eq!(error.to_string(), "date out of range");
        let error = error.at(7);
        assert_eq!(error.slot(), Some(7));
        assert_eq!(error.kind(), ErrorKind::OutOfRange);
        assert_eq!(error.message(), "date out of range");
        assert_eq!(error.to_string(), "date out of range (slot 7)");
    }
}
