//! Spilled bytes that are not what was written: a block, a packed body or
//! a chunk of columns read back that fails its checks. The caller reports
//! it as damaged data (SQLSTATE XX001, `data_corrupted`), apart from its
//! own misuse of a call, which stays an internal error.

use std::fmt;

/// Spilled bytes read back that fail their checks, with what failed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Damaged(pub String);

impl fmt::Display for Damaged {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Damaged {}

/// As `anyhow::ensure!`, failing with [`Damaged`]: for a check of bytes
/// read back.
macro_rules! intact {
    ($cond:expr, $($arg:tt)+) => {
        if !$cond {
            return Err($crate::Damaged(format!($($arg)+)).into());
        }
    };
}
pub(crate) use intact;
