//! What the table's own tests and its loom models share: keys of their
//! own, which read no columns.

use super::keys::KeySource;
#[cfg(test)]
use crate::error::{Kind, Result, Rule};
use crate::ones;

/// The kind and rule of a call's error, `None` when it succeeded.
#[cfg(test)]
pub(crate) fn refusal<T>(result: Result<T>) -> Option<(Kind, Rule)> {
    result.err().map(|error| (error.kind(), error.rule()))
}

/// One key of int4 values with no NULL: the keys of the table's own tests
/// and of its loom model, which read no columns.
pub(crate) struct IntKeys<'a>(pub(crate) &'a [i32]);

impl KeySource for IntKeys<'_> {
    fn nkeys(&self) -> usize {
        1
    }

    fn nrows(&self) -> usize {
        self.0.len()
    }

    fn word(&self, _: usize, index: usize, selected: u64, out: &mut [i64; 64]) -> Option<u64> {
        for bit in ones(selected) {
            out[bit] = i64::from(self.0[index * 64 + bit]);
        }
        Some(selected)
    }
}
