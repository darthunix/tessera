#![forbid(unsafe_code)]

use crate::error::{Error, Result, Rule, ensure};

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
        Error::wrong_call(Rule::MaskWords)
            .with(expected)
            .and(words.len())
    );
    let tail = nrows % 64;
    ensure!(
        tail == 0 || words[expected - 1] >> tail == 0,
        Error::wrong_call(Rule::MaskPadding)
    );
    Ok(())
}

/// Reject a row that is not below `nrows`.
#[inline]
fn validate_row(row: usize, nrows: usize) -> Result<()> {
    ensure!(
        row < nrows,
        Error::wrong_call(Rule::Row).with(row).and(nrows)
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
/// of the last word past the last row are 0; a mask of other words is
/// refused, its words left as they were, and a mask a call writes keeps
/// those bits 0:
///
/// ```text
///  a mask of 70 rows made of words: two words
///  word 0   bits 0..=63    rows 0..=63
///  word 1   bits 0..=5     rows 64..=69
///           bits 6..=63    0, always
/// ```
///
/// The view borrows the caller's words; it neither copies nor frees them.
///
/// ```
/// use relhash::RowMaskView;
///
/// // Rows 0, 2 and 64 of a batch of 70 rows: two words, row `r` the bit
/// // `1 << (r % 64)` of word `r / 64`.
/// let words = [1 << 0 | 1 << 2, 1 << (64 - 64)];
/// let rows = RowMaskView::try_new(70, &words)?;
/// assert_eq!(rows.selected_rows().collect::<Vec<_>>(), [0, 2, 64]);
/// // One word is not enough for 70 rows, and row 70 is past the batch.
/// assert!(RowMaskView::try_new(70, &[1 << 0 | 1 << 2]).is_err());
/// assert!(RowMaskView::try_new(70, &[0, 1 << (70 - 64)]).is_err());
/// # Ok::<(), relhash::Error>(())
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
/// as [`RowMaskView::try_new`] does. No operation allocates, an error
/// included, and none frees or resizes the backing storage. Dropping the mask ends the borrow;
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

    /// The words, for a call that narrows them in place: a word written
    /// holds only rows it held, so the bits past the last row stay 0.
    #[inline]
    pub(crate) fn words_mut(&mut self) -> &mut [u64] {
        self.words
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
    /// // Rows 0, 1, 2 and 64 of 65; then rows 0 and 2 of the first word.
    /// let mut words = [1 << 0 | 1 << 1 | 1 << 2, 1 << (64 - 64)];
    /// let mut rows = RowMask::try_new(65, &mut words)?;
    /// rows.intersect_word(0, 1 << 0 | 1 << 2)?;
    /// rows.intersect_word(1, u64::MAX)?;
    /// assert_eq!(rows.as_view().selected_rows().collect::<Vec<_>>(), [0, 2, 64]);
    /// # Ok::<(), relhash::Error>(())
    /// ```
    #[inline]
    pub fn intersect_word(&mut self, index: usize, bits: u64) -> Result<()> {
        let word = self
            .words
            .get_mut(index)
            .ok_or(Error::wrong_call(Rule::MaskWord))?;
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
    /// // Rows 0 and 2, then row 64; row 65 is past the batch of 65 rows.
    /// rows.set_word(0, 1 << 0 | 1 << 2)?;
    /// rows.set_word(1, 1 << (64 - 64))?;
    /// assert!(rows.set_word(1, 1 << (65 - 64)).is_err());
    /// assert_eq!(rows.as_view().selected_rows().collect::<Vec<_>>(), [0, 2, 64]);
    /// # Ok::<(), relhash::Error>(())
    /// ```
    #[inline]
    pub fn set_word(&mut self, index: usize, bits: u64) -> Result<()> {
        let word = self
            .words
            .get_mut(index)
            .ok_or(Error::wrong_call(Rule::MaskWord))?;
        let valid = (self.nrows - index * 64).min(64);
        ensure!(
            valid == 64 || bits >> valid == 0,
            Error::wrong_call(Rule::WordPadding)
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
            Error::wrong_call(Rule::MaskRows)
                .with(self.nrows)
                .and(other.nrows)
        );
        for (word, other) in self.words.iter_mut().zip(other.words()) {
            *word &= other;
        }
        Ok(())
    }
}

/// Check that the masks and arrays of a call all have its `nrows` rows:
/// `counts` holds their row counts.
///
/// A count other than `nrows` returns an error with `nrows` and that
/// count, so that a call fails before it changes anything.
#[inline]
pub(crate) fn check_rows(nrows: usize, counts: &[usize]) -> Result<()> {
    match counts.iter().find(|&&rows| rows != nrows) {
        None => Ok(()),
        Some(&rows) => Err(Error::wrong_call(Rule::RowCounts).with(nrows).and(rows)),
    }
}

#[cfg(test)]
mod tests {
    use super::{RowMask, RowMaskView, word_count};
    use crate::{Kind, Rule};

    /// A mask's words read back as they are, none past them; a row taken
    /// out clears its bit alone; an intersection keeps the rows of both.
    #[test]
    fn a_mask_reads_its_words_and_takes_rows_out() {
        let words = [0xf0f0_f0f0_0f0f_0f0f, 0b10_1101];
        let view = RowMaskView::try_new(70, &words).unwrap();
        assert_eq!(
            (view.get_word(0), view.get_word(1), view.get_word(2)),
            (Some(words[0]), Some(words[1]), None)
        );
        let mut written = words;
        let mut mask = RowMask::try_new(70, &mut written).unwrap();
        mask.remove(66);
        mask.remove(1);
        assert_eq!(
            mask.as_view().words().collect::<Vec<_>>(),
            [words[0] & !0b10, words[1] & !0b100]
        );
        let other = [0x00ff_00ff_00ff_00ff, 0b11_1000];
        mask.intersect(RowMaskView::try_new(70, &other).unwrap())
            .unwrap();
        assert_eq!(
            mask.as_view().words().collect::<Vec<_>>(),
            [words[0] & !0b10 & other[0], words[1] & !0b100 & other[1]]
        );
    }

    /// A mask of 70 rows made of one word, of three, or of two with bit 6
    /// of the second set is refused, read or written, and its words are
    /// left as they were.
    #[test]
    fn a_mask_of_wrong_words_is_refused_and_left_as_it_was() {
        for (words, rule) in [
            (&[u64::MAX][..], Rule::MaskWords),
            (&[u64::MAX; 3][..], Rule::MaskWords),
            (&[u64::MAX, 1 << 6][..], Rule::MaskPadding),
        ] {
            let read = RowMaskView::try_new(70, words)
                .err()
                .map(|error| (error.kind(), error.rule()));
            assert_eq!(read, Some((Kind::WrongCall, rule)), "{words:x?}");
            let mut written = words.to_vec();
            let refused = RowMask::try_new(70, &mut written)
                .err()
                .map(|error| (error.kind(), error.rule()));
            assert_eq!(refused, Some((Kind::WrongCall, rule)), "{words:x?}");
            assert_eq!(written, words, "the words are left as they were");
        }
    }

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
