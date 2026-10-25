//! The backend of a system the crate cannot count on: `open` fails, and
//! no `Counters` of it can exist.

use crate::{Error, ErrorKind, Reading};

/// Counters of a system without a backend: a type with no values.
pub enum Counters {}

impl Counters {
    pub fn open() -> Result<Self, Error> {
        Err(Error::new(
            ErrorKind::Unsupported,
            "pmu counts only on macOS and on Linux",
        ))
    }

    pub fn read(&self) -> Result<Reading, Error> {
        match *self {}
    }
}

pub fn cpu_number() -> Option<usize> {
    None
}
