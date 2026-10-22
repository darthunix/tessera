//! What the table's own tests and its loom models share: keys of their
//! own, which read no columns.

// Without `alloc` the table's tests are left out, and with them most of
// what this module gives them.
#![cfg_attr(not(feature = "alloc"), allow(dead_code))]

use super::keys::KeySource;
#[cfg(test)]
use crate::error::{Kind, Rule};
use crate::ones;

/// The kind and rule of a call's error, through any context a test added
/// to it; `None` when it succeeded, or failed with an error not the
/// crate's.
#[cfg(test)]
pub(crate) fn refusal<T, E: Into<anyhow::Error>>(
    result: core::result::Result<T, E>,
) -> Option<(Kind, Rule)> {
    let error = result.err()?.into();
    let error = error.downcast_ref::<crate::Error>()?;
    Some((error.kind(), error.rule()))
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

/// A key column of the table's own tests: each row's value as its slot
/// holds it, and the rows that are not NULL, every row when `None`.
#[cfg(test)]
pub(crate) struct Column {
    slots: Vec<i64>,
    non_null: Option<Vec<u64>>,
}

#[cfg(test)]
impl Column {
    /// A column of `values`, int4 or int8 alike, NULL where `non_null`
    /// has no bit.
    pub(crate) fn new<T: Copy + Into<i64>>(values: &[T], non_null: Option<&[u64]>) -> Self {
        Self {
            slots: values.iter().map(|&value| value.into()).collect(),
            non_null: non_null.map(<[u64]>::to_vec),
        }
    }
}

/// Columns of keys, one key each, of any count and with NULLs: the source
/// of keys of the table's own tests.
#[cfg(test)]
pub(crate) struct Keys<'a>(pub(crate) &'a [Column]);

#[cfg(test)]
impl KeySource for Keys<'_> {
    fn nkeys(&self) -> usize {
        self.0.len()
    }

    fn nrows(&self) -> usize {
        self.0.first().map_or(0, |column| column.slots.len())
    }

    fn word(&self, key: usize, index: usize, selected: u64, out: &mut [i64; 64]) -> Option<u64> {
        let column = &self.0[key];
        let non_null = match &column.non_null {
            Some(words) => selected & words.get(index)?,
            None => selected,
        };
        for bit in ones(selected) {
            let slot = *column.slots.get(index * 64 + bit)?;
            out[bit] = if non_null >> bit & 1 == 1 { slot } else { 0 };
        }
        Some(non_null)
    }
}

/// A source of keys that cannot give word `.1` of the batch and gives the
/// others as `.0` does: a call that fails part way.
#[cfg(test)]
pub(crate) struct Failing<'a, K: ?Sized>(pub(crate) &'a K, pub(crate) usize);

#[cfg(test)]
impl<K: KeySource + ?Sized> KeySource for Failing<'_, K> {
    fn nkeys(&self) -> usize {
        self.0.nkeys()
    }

    fn nrows(&self) -> usize {
        self.0.nrows()
    }

    fn word(&self, key: usize, index: usize, selected: u64, out: &mut [i64; 64]) -> Option<u64> {
        if index == self.1 {
            return None;
        }
        self.0.word(key, index, selected, out)
    }
}
