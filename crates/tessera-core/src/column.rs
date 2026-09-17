use anyhow::{Result, ensure};

use crate::bitmap::validate_row;
use crate::{ColumnReader, RowMaskView, WordBlock, WordValues};

/// Read-only access to borrowed, initialized values and their nullness.
///
/// The view neither copies values nor owns or frees their storage. Every
/// element of the Rust slice must be initialized, including positions marked
/// NULL; those elements are not exposed by [`Self::get`]. Active-row selection
/// is separate, and indices always refer to physical rows.
///
/// The optional `non_nulls` mask is a [`RowMaskView`] with set bits for
/// non-NULL values. Without this mask, every value is non-NULL.
///
/// This is not an adapter for `TessDatumColumn`: its unrequested positions
/// may be uninitialized, and such storage cannot be borrowed as `&[T]`.
///
/// A column cannot outlive its values:
///
/// ```compile_fail
/// use tessera_core::ColumnView;
/// let column;
/// {
///     let values = [String::from("borrowed")];
///     column = ColumnView::try_new(&values, None).unwrap();
/// }
/// assert!(column.get(0).unwrap().is_some());
/// ```
///
/// A column's non-NULL mask words cannot be modified while borrowed:
///
/// ```compile_fail
/// use tessera_core::{ColumnView, RowMaskView};
/// let values = [10];
/// let mut words = [1];
/// let non_nulls = RowMaskView::try_new(1, &words).unwrap();
/// let column = ColumnView::try_new(&values, Some(non_nulls)).unwrap();
/// words[0] = 0;
/// assert!(column.get(0).unwrap().is_some());
/// ```
#[derive(Debug)]
pub struct ColumnView<'a, T> {
    values: &'a [T],
    non_nulls: Option<RowMaskView<'a>>,
}

impl<T: Copy> ColumnReader for ColumnView<'_, T> {
    type Value = T;

    fn nrows(&self) -> usize {
        self.nrows()
    }

    #[inline]
    fn get(&self, row: usize) -> Result<Option<T>> {
        Ok(self.get(row)?.copied())
    }

    #[inline]
    fn word_values(
        &self,
        word_index: usize,
        selected: u64,
    ) -> Result<impl Iterator<Item = (usize, Option<T>)> + '_> {
        let non_nulls = self
            .non_nulls
            .map_or(u64::MAX, |mask| mask.word(word_index).unwrap_or(0));
        WordValues::try_new(
            self.nrows(),
            word_index,
            selected,
            u64::MAX,
            move |row: usize| (non_nulls & (1 << (row % 64)) != 0).then(|| self.values[row]),
        )
    }

    /// Every row is prepared and initialized, so every full word is a block.
    #[inline]
    fn word_block(&self, word_index: usize) -> Option<WordBlock<'_, T>> {
        let base = word_index.checked_mul(64)?;
        let values = self
            .values
            .get(base..base.checked_add(64)?)?
            .try_into()
            .ok()?;
        let non_nulls = self
            .non_nulls
            .map_or(u64::MAX, |mask| mask.word(word_index).unwrap_or(0));
        Some(WordBlock::Dense { values, non_nulls })
    }
}

impl<'a, T> ColumnView<'a, T> {
    /// Borrow values and a non-NULL mask, rejecting different physical row counts.
    ///
    /// `None` means all values are non-NULL; their count comes from `values`.
    pub fn try_new(values: &'a [T], non_nulls: Option<RowMaskView<'a>>) -> Result<Self> {
        if let Some(non_nulls) = non_nulls {
            ensure!(
                values.len() == non_nulls.nrows(),
                "expected {} physical rows, got {}",
                values.len(),
                non_nulls.nrows()
            );
        }
        Ok(Self { values, non_nulls })
    }

    /// Return the number of physical rows, including null values.
    pub fn nrows(&self) -> usize {
        self.values.len()
    }

    /// Return the original value's reference, or `None` for NULL.
    ///
    /// An out-of-bounds index returns an error, distinct from NULL. Nullness
    /// is checked before accessing the values slice. No `Copy` bound is needed.
    pub fn get(&self, row: usize) -> Result<Option<&T>> {
        match self.non_nulls {
            Some(mask) if !mask.contains(row)? => return Ok(None),
            Some(_) => {}
            None => validate_row(row, self.values.len())?,
        }
        Ok(Some(&self.values[row]))
    }
}
