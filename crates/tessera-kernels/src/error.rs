//! The errors a kernel reports with an SQLSTATE of their own, and the one
//! place that tells them from the rest: a value of the data, as the
//! function a kernel implements raises it, or spilled bytes read back
//! damaged ([`tessera_spill::Damaged`]). Any other error is a misuse of a
//! call, which the C boundary reports as an internal error.

use tessera_spill::Damaged;

use crate::calendar::CalendarError;
use crate::ops::ArithmeticError;
use crate::text::TextError;

/// How the C boundary reports a classified error (`TessStatusCode`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorKind {
    /// An int4 or int8 result does not fit (SQLSTATE 22003).
    IntegerOutOfRange,
    /// A zero divisor (22012).
    DivisionByZero,
    /// Another error of the data, with the function's SQLSTATE.
    Data,
    /// Spilled bytes read back that fail their checks (XX001).
    Damaged,
}

/// An error with an SQLSTATE of its own.
pub trait SqlError: std::error::Error + Send + Sync + 'static {
    /// The five-character SQLSTATE.
    fn sqlstate(&self) -> &'static str;
    /// How the C boundary reports it.
    fn kind(&self) -> ErrorKind;
}

impl SqlError for ArithmeticError {
    fn sqlstate(&self) -> &'static str {
        ArithmeticError::sqlstate(*self)
    }

    fn kind(&self) -> ErrorKind {
        match self {
            Self::IntegerOutOfRange | Self::BigintOutOfRange => ErrorKind::IntegerOutOfRange,
            Self::DivisionByZero => ErrorKind::DivisionByZero,
        }
    }
}

impl SqlError for CalendarError {
    fn sqlstate(&self) -> &'static str {
        CalendarError::sqlstate(*self)
    }

    fn kind(&self) -> ErrorKind {
        ErrorKind::Data
    }
}

impl SqlError for TextError {
    fn sqlstate(&self) -> &'static str {
        TextError::sqlstate(*self)
    }

    fn kind(&self) -> ErrorKind {
        ErrorKind::Data
    }
}

impl SqlError for Damaged {
    fn sqlstate(&self) -> &'static str {
        "XX001"
    }

    fn kind(&self) -> ErrorKind {
        ErrorKind::Damaged
    }
}

/// The error with an SQLSTATE behind `error`, through any context added
/// to it, or `None` for a misuse of a call.
pub fn classify(error: &anyhow::Error) -> Option<&dyn SqlError> {
    if let Some(arithmetic) = error.downcast_ref::<ArithmeticError>() {
        return Some(arithmetic);
    }
    if let Some(calendar) = error.downcast_ref::<CalendarError>() {
        return Some(calendar);
    }
    if let Some(text) = error.downcast_ref::<TextError>() {
        return Some(text);
    }
    error
        .downcast_ref::<Damaged>()
        .map(|damaged| damaged as &dyn SqlError)
}
