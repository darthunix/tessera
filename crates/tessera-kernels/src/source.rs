//! What the batch functions of a family share: a [`Source`] of a row's
//! value, a column or a scalar, and the loop that runs a function over the
//! selected rows whose arguments are not NULL, in the order of the rows.

use std::mem::MaybeUninit;

use anyhow::Result;
use tessera_core::{RowMask, RowMaskView, check_rows, ones};

/// A column or a scalar of a batch function: a row's value, `None` for
/// NULL. The batch functions ask only for selected rows.
pub trait Source<T> {
    /// The value of a row.
    fn get(&self, row: usize) -> Option<T>;
}

/// Run `apply` over the selected rows whose arguments are not NULL, in the
/// order of the rows, writing each result and `non_nulls`; the first error
/// fails the call, the outputs then unspecified.
#[inline(always)]
pub(crate) fn map_rows<L: Copy, R: Copy, T, E>(
    left: &impl Source<L>,
    right: &impl Source<R>,
    rows: RowMaskView<'_>,
    values: &mut [MaybeUninit<T>],
    non_nulls: &mut RowMask<'_>,
    apply: impl Fn(L, R) -> Result<T, E>,
) -> Result<()>
where
    E: std::error::Error + Send + Sync + 'static,
{
    check_rows(rows.nrows(), &[values.len(), non_nulls.as_view().nrows()])?;
    for word in 0..rows.nrows().div_ceil(64) {
        let look = rows.word(word);
        let mut present = 0;
        for bit in ones(look) {
            let row = word * 64 + bit;
            let (Some(left), Some(right)) = (left.get(row), right.get(row)) else {
                continue;
            };
            values[row].write(apply(left, right)?);
            present |= 1 << bit;
        }
        non_nulls.set_word(word, present)?;
    }
    Ok(())
}

/// The second argument of a function of one: a placeholder every row has.
pub(crate) struct Unary;

impl Source<()> for Unary {
    #[inline(always)]
    fn get(&self, _row: usize) -> Option<()> {
        Some(())
    }
}
