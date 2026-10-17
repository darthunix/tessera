//! What the table's own tests and its loom models share: keys of their
//! own, which read no columns.

use anyhow::Result;

use super::keys::KeySource;
use crate::ones;

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

    fn word(&self, _: usize, index: usize, selected: u64, out: &mut [i64; 64]) -> Result<u64> {
        for bit in ones(selected) {
            out[bit] = i64::from(self.0[index * 64 + bit]);
        }
        Ok(selected)
    }
}
