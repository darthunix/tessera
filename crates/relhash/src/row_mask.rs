use anyhow::{Context, Result, anyhow, ensure};

use crate::bits::ones;

/// The words of a mask of `nrows` rows.
#[inline]
fn word_count(nrows: usize) -> usize {
    nrows.div_ceil(64)
}

/// Reject words that are not the `nrows` rows of a mask: a word count of
/// another mask, or a bit set past the last row.
#[inline]
fn validate_words(nrows: usize, words: &[u64]) -> Result<()> {
    let expected = word_count(nrows);
    ensure!(
        words.len() == expected,
        "expected {expected} bitmap words, got {}",
        words.len()
    );
    let tail = nrows % 64;
    ensure!(
        tail == 0 || words[expected - 1] >> tail == 0,
        "bitmap has set bits beyond its physical row count"
    );
    Ok(())
}

/// Reject a row that is not below `nrows`.
#[inline]
fn validate_row(row: usize, nrows: usize) -> Result<()> {
    ensure!(
        row < nrows,
        "physical row {row} is out of bounds for {nrows} rows"
    );
    Ok(())
}

/// A mask of rows, to read: which rows of a batch a call takes.
///
/// A batch of `nrows` rows has one bit per row, and a bit that is 1 chooses
/// its row. The same type also tells which values of a column are not NULL:
/// a bit that is 1 then means the row has a value. The bits lie in 64-bit
/// words, lowest bit first: word 0 holds rows 0 to 63, word 1 rows 64 to
/// 127, and so on. A mask has exactly the words its rows need, and the bits
/// of the last word past the last row are 0. The view borrows the caller's
/// words, which the C API's `TessRowMask` holds too; it neither copies nor
/// frees them.
///
/// ```
/// use relhash::RowMaskView;
///
/// // Rows 0, 2 and 64 of a batch of 70 rows: two words.
/// let words = [0b101, 0b1];
/// let rows = RowMaskView::try_new(70, &words)?;
/// assert_eq!(rows.selected_rows().collect::<Vec<_>>(), [0, 2, 64]);
/// // One word is not enough for 70 rows, and row 70 is past the batch.
/// assert!(RowMaskView::try_new(70, &[0b101]).is_err());
/// assert!(RowMaskView::try_new(70, &[0, 1 << 6]).is_err());
/// # Ok::<(), anyhow::Error>(())
/// ```
///
/// A view cannot outlive the words it borrows. Here `words` is one word
/// whose bit 0 chooses the one row of the batch, and it is gone before the
/// view is read, so the compiler refuses the code:
///
/// ```compile_fail
/// use relhash::RowMaskView;
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
    #[inline]
    pub fn try_new(nrows: usize, words: &'a [u64]) -> Result<Self> {
        validate_words(nrows, words)?;
        Ok(Self { nrows, words })
    }

    /// Word `word_index` of the mask, or `None` past its last word, as a
    /// slice's `get`. The last word's bits past the last row are 0.
    #[inline]
    pub fn get_word(&self, word_index: usize) -> Option<u64> {
        (word_index < word_count(self.nrows)).then(|| self.word(word_index))
    }

    /// Word `word_index` of the mask, for a loop over its words: an index
    /// past the last word panics, as an index past a slice does.
    ///
    /// It has no range check of its own on purpose. A loop that writes its
    /// results through `&mut` cannot keep the row count in a register, and a
    /// check reloads it every word: 4-9 instructions a word in the
    /// arithmetic and hash kernels, by PMU.
    ///
    /// The last word's bits past the last row are 0.
    #[inline]
    pub fn word(&self, word_index: usize) -> u64 {
        debug_assert!(word_index < word_count(self.nrows));
        self.words[word_index]
    }

    /// The logical 64-row words in order, the last one's padding zero.
    #[inline]
    pub fn words(&self) -> impl ExactSizeIterator<Item = u64> + 'a {
        self.words.iter().copied()
    }

    /// Return the number of physical rows, including unselected rows.
    pub fn nrows(&self) -> usize {
        self.nrows
    }

    /// Return the number of selected rows, not the physical row count.
    pub fn selected_count(&self) -> usize {
        self.words().map(|word| word.count_ones() as usize).sum()
    }

    /// Check a physical row, returning an error if `row >= self.nrows()`.
    #[inline]
    pub fn contains(&self, row: usize) -> Result<bool> {
        validate_row(row, self.nrows)?;
        Ok(self.words[row / 64] & (1_u64 << (row % 64)) != 0)
    }

    /// The numbers of the chosen rows, from 0 to `nrows - 1`, lowest first,
    /// without allocating.
    pub fn selected_rows(&self) -> impl Iterator<Item = usize> + '_ {
        self.words()
            .enumerate()
            .flat_map(|(word, bits)| ones(bits).map(move |bit| word * 64 + bit))
    }
}

/// A mask of rows that a call changes: it removes rows ([`Self::remove`],
/// [`Self::intersect_word`]) or writes the rows of its result a word at a
/// time ([`Self::set_word`]). The words are those of [`RowMaskView`], which
/// [`Self::as_view`] reads. The mask, not a word alone, takes these calls:
/// it knows its row count, so it refuses a word past its end and a bit past
/// its last row, and its words keep the form of a mask.
///
/// It borrows the caller's words exclusively, and construction checks them
/// as [`RowMaskView::try_new`] does. Successful
/// operations do not allocate; creating an error may allocate. No operation
/// frees or resizes the backing storage. Dropping the mask ends the borrow;
/// changes remain in the caller's words.
///
/// Two mutable views cannot access the same storage simultaneously:
///
/// ```compile_fail
/// use relhash::RowMask;
/// let mut words = [1];
/// let mut first = RowMask::try_new(1, &mut words).unwrap();
/// let second = RowMask::try_new(1, &mut words).unwrap();
/// first.remove(0);
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
    #[inline]
    pub fn try_new(nrows: usize, words: &'a mut [u64]) -> Result<Self> {
        validate_words(nrows, words)?;
        Ok(Self { nrows, words })
    }

    /// Reborrow for reading; mutation is forbidden while that view is in use.
    ///
    /// ```compile_fail
    /// use relhash::RowMask;
    /// let mut words = [1];
    /// let mut rows = RowMask::try_new(1, &mut words).unwrap();
    /// let view = rows.as_view();
    /// rows.remove(0);
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
    /// The bits past the last row are already 0, so removing such a row
    /// leaves the words unchanged; a row past the words is ignored too.
    #[inline]
    pub fn remove(&mut self, row: usize) {
        if let Some(word) = self.words.get_mut(row / 64) {
            *word &= !(1_u64 << (row % 64));
        }
    }

    /// Intersect one 64-row word, without restoring any cleared row.
    ///
    /// An absent word returns an error before any mutation, even when `bits`
    /// is zero. Other words stay unchanged. Padding bits in `bits` are harmless:
    /// intersection preserves the zero padding validated by the constructor.
    ///
    /// ```
    /// use relhash::RowMask;
    /// let mut words = [0b111, 1];
    /// let mut rows = RowMask::try_new(65, &mut words)?;
    /// rows.intersect_word(0, 0b101)?;
    /// rows.intersect_word(1, u64::MAX)?;
    /// assert_eq!(rows.as_view().selected_rows().collect::<Vec<_>>(), [0, 2, 64]);
    /// # Ok::<(), anyhow::Error>(())
    /// ```
    #[inline]
    pub fn intersect_word(&mut self, index: usize, bits: u64) -> Result<()> {
        let word = self
            .words
            .get_mut(index)
            .context("mask word is out of bounds")?;
        *word &= bits;
        Ok(())
    }

    /// Replace one 64-row word; this is the one operation that can restore
    /// rows, for masks a kernel produces rather than narrows.
    ///
    /// An absent word or set padding bits in `bits` return an error before
    /// any mutation. Other words stay unchanged.
    ///
    /// ```
    /// use relhash::RowMask;
    /// let mut words = [0, 0];
    /// let mut rows = RowMask::try_new(65, &mut words)?;
    /// rows.set_word(0, 0b101)?;
    /// rows.set_word(1, 1)?;
    /// assert!(rows.set_word(1, 0b10).is_err());
    /// assert_eq!(rows.as_view().selected_rows().collect::<Vec<_>>(), [0, 2, 64]);
    /// # Ok::<(), anyhow::Error>(())
    /// ```
    #[inline]
    pub fn set_word(&mut self, index: usize, bits: u64) -> Result<()> {
        let word = self
            .words
            .get_mut(index)
            .context("mask word is out of bounds")?;
        let valid = (self.nrows - index * 64).min(64);
        ensure!(
            valid == 64 || bits >> valid == 0,
            "mask word has set padding bits"
        );
        *word = bits;
        Ok(())
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
        for (word, other) in self.words.iter_mut().zip(other.words()) {
            *word &= other;
        }
        Ok(())
    }
}

/// Check that the masks and arrays of a call all have its `nrows` rows:
/// `counts` holds their row counts, and `call` names the kind of call in
/// the error.
///
/// A count other than `nrows` returns an error, so that a call fails
/// before it changes anything.
#[inline]
pub fn check_rows(nrows: usize, counts: &[usize], call: &str) -> Result<()> {
    if counts.iter().all(|&rows| rows == nrows) {
        Ok(())
    } else {
        Err(rows_differ(call))
    }
}

/// The error of [`check_rows`], built out of line so that a call's check
/// stays a comparison and a branch.
#[cold]
#[inline(never)]
fn rows_differ(call: &str) -> anyhow::Error {
    anyhow!("the masks of a {call} call have different row counts")
}

#[cfg(test)]
mod tests {
    use super::word_count;

    #[test]
    fn word_count_does_not_overflow() {
        for (nrows, expected) in [
            (0, 0),
            (1, 1),
            (63, 1),
            (64, 1),
            (65, 2),
            (usize::MAX - 63, usize::MAX / 64),
            (usize::MAX - 62, usize::MAX / 64 + 1),
            (usize::MAX, usize::MAX / 64 + 1),
        ] {
            assert_eq!(word_count(nrows), expected);
        }
    }
}
