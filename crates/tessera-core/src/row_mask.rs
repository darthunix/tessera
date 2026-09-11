use anyhow::{Result, ensure};

use crate::bitmap::{validate_row, validate_words};

/// A read-only, borrowed bitmap of physical rows.
///
/// Set bits can represent active rows or non-NULL values. Row selection and
/// non-NULL tracking use separate bitmaps, even though they share this view type.
///
/// The least significant bit of each word represents its first row. Words
/// beyond the required length and set padding bits are rejected. Zero rows
/// require an empty slice. This matches `TessRowMask` bit numbering, not its
/// C struct layout. The view neither owns nor modifies its backing words.
///
/// A view cannot outlive its storage:
///
/// ```compile_fail
/// use tessera_core::RowMaskView;
/// let rows;
/// {
///     let words = [1];
///     rows = RowMaskView::try_new(1, &words).unwrap();
/// }
/// assert_eq!(rows.selected_count(), 1);
/// ```
#[derive(Clone, Copy, Debug)]
pub struct RowMaskView<'a> {
    nrows: usize,
    words: &'a [u64],
}

impl<'a> RowMaskView<'a> {
    /// Borrow words, rejecting an incorrect word count or nonzero padding bits.
    pub fn try_new(nrows: usize, words: &'a [u64]) -> Result<Self> {
        validate_words(nrows, words)?;
        Ok(Self { nrows, words })
    }

    /// Return the number of physical rows, including unselected rows.
    pub fn nrows(&self) -> usize {
        self.nrows
    }

    /// Return the number of selected rows, not the physical row count.
    pub fn selected_count(&self) -> usize {
        self.words
            .iter()
            .map(|word| word.count_ones() as usize)
            .sum()
    }

    /// Check a physical row, returning an error if `row >= self.nrows()`.
    pub fn contains(&self, row: usize) -> Result<bool> {
        validate_row(row, self.nrows)?;
        Ok(self.words[row / 64] & (1_u64 << (row % 64)) != 0)
    }

    /// Visit selected physical indices in increasing order without allocating.
    ///
    /// Only set bits are visited within each word. Once exhausted, the
    /// iterator keeps returning `None`.
    pub fn selected_indices(&self) -> impl Iterator<Item = usize> + '_ {
        self.words.iter().enumerate().flat_map(|(word, &bits)| {
            let mut remaining = bits;
            std::iter::from_fn(move || {
                if remaining == 0 {
                    return None;
                }
                let row = word * 64 + remaining.trailing_zeros() as usize;
                remaining &= remaining - 1;
                Some(row)
            })
        })
    }
}

/// An exclusively borrowed row bitmap that can only remove rows.
///
/// Construction validates the same invariants as [`RowMaskView`]. Successful
/// operations do not allocate; creating an error may allocate. No operation
/// frees or resizes the backing storage. Dropping the mask ends the borrow;
/// changes remain in the caller's words.
///
/// Two mutable views cannot access the same storage simultaneously:
///
/// ```compile_fail
/// use tessera_core::RowMask;
/// let mut words = [1];
/// let mut first = RowMask::try_new(1, &mut words).unwrap();
/// let second = RowMask::try_new(1, &mut words).unwrap();
/// first.clear(0);
/// assert_eq!(second.as_view().selected_count(), 0);
/// ```
#[derive(Debug)]
pub struct RowMask<'a> {
    nrows: usize,
    words: &'a mut [u64],
}

impl<'a> RowMask<'a> {
    /// Borrow words exclusively, rejecting bad dimensions or padding bits.
    ///
    /// Validation never modifies the supplied words, including on error.
    pub fn try_new(nrows: usize, words: &'a mut [u64]) -> Result<Self> {
        validate_words(nrows, words)?;
        Ok(Self { nrows, words })
    }

    /// Reborrow for reading; mutation is forbidden while that view is in use.
    ///
    /// ```compile_fail
    /// use tessera_core::RowMask;
    /// let mut words = [1];
    /// let mut rows = RowMask::try_new(1, &mut words).unwrap();
    /// let view = rows.as_view();
    /// rows.clear(0);
    /// assert_eq!(view.selected_count(), 1);
    /// ```
    pub fn as_view(&self) -> RowMaskView<'_> {
        RowMaskView {
            nrows: self.nrows,
            words: self.words,
        }
    }

    /// Remove a physical row; an absent or out-of-bounds row is ignored.
    ///
    /// Padding bits are already zero after construction, so clearing one
    /// leaves the words unchanged. Indices beyond the words are also ignored.
    #[inline]
    pub fn clear(&mut self, row: usize) {
        if let Some(word) = self.words.get_mut(row / 64) {
            *word &= !(1_u64 << (row % 64));
        }
    }

    /// Remove rows absent from `other`, without restoring any cleared row.
    ///
    /// Different physical row counts return an error before any mutation.
    pub fn intersect(&mut self, other: RowMaskView<'_>) -> Result<()> {
        ensure!(
            self.nrows == other.nrows,
            "expected {} physical rows, got {}",
            self.nrows,
            other.nrows
        );
        for (word, keep) in self.words.iter_mut().zip(other.words) {
            *word &= keep;
        }
        Ok(())
    }
}
